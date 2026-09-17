//! The harness-agnostic seam. The trait has been here from the first commit
//! because adapters are the part that rots; what a harness is called, where it
//! keeps its state, and what it must find in that state directory all live
//! behind it rather than being spelled with one harness's name throughout the
//! node. That seam is what let the omp harness be removed at the OpenCode
//! cutover without the rest of the node noticing.

pub mod claude;
pub mod opencode;
pub mod types;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::gateway::model::Wiring;
use crate::runner::Runner;

/// Where a harness keeps its state inside the runner, and what it calls that
/// place. The node mounts a directory it owns at `dir` under the harness's
/// home and sets `env` to the same path, so the mount and the harness's own
/// idea of its state directory cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Directory under the harness's home, including the leading dot.
    pub dir: &'static str,
    /// The environment variable that names it. Empty when the harness has no
    /// such variable — OpenCode decides where its state lives from `HOME` and
    /// the XDG directories, which its adapter sets per session — and then the
    /// runners set none rather than inventing one.
    pub env: &'static str,
}

/// The harness ids this node has an adapter for.
pub const KNOWN: &[&str] = &["opencode", "claude"];

/// The harness this node ran until the 2026-09-13 cutover. A `node.toml`
/// that still names it is migrated to OpenCode as it loads
/// (`crate::legacy::migrate_config`); a config that reaches the adapter
/// saying `omp` anyway reads the migration path rather than a bare "no
/// adapter for harness `omp`".
pub const RETIRED: &str = "omp";

/// What that operator is told. The sessions omp ran are not lost — they are
/// archived read-only, keeping their harness identity and version — and the
/// way to carry one forward is a new session under a supported harness, not a
/// relaunch of a harness that is gone.
pub const RETIRED_MESSAGE: &str = concat!(
    "the `omp` harness was removed at the OpenCode cutover. ",
    "A node.toml that names it is migrated to \"opencode\" when the node loads it; ",
    "this configuration was not. ",
    "Set [harness] id = \"opencode\" (or \"claude\") in node.toml, then run ",
    "`tracon setup` to build the image and `tracon session archive-legacy` to ",
    "put the omp sessions away read-only. Carry one forward with ",
    "`tracon session reopen <id> --harness opencode`."
);

/// The layout for a harness id, for the few callers that have the config but
/// not the adapter (the boundary preflight). An id that is not Claude's gets
/// OpenCode's, which only that preflight can reach: `adapter_for` refuses an
/// unknown id first, so no session ever runs against a layout that is not its
/// harness's.
pub fn layout(harness_id: &str) -> Layout {
    if harness_id == claude::ClaudeAdapter::ID {
        return claude::ClaudeAdapter::layout();
    }
    opencode::OpenCodeAdapter::layout()
}

/// The adapter for the configured harness. An unknown id fails here, at
/// startup, rather than silently running whichever harness happens to be the
/// default — a node that thinks it is running something else is worse than a
/// node that will not start.
pub fn adapter_for(cfg: &Config) -> Result<Arc<dyn HarnessAdapter>, AdapterError> {
    match cfg.harness.id.as_str() {
        claude::ClaudeAdapter::ID => Ok(Arc::new(claude::ClaudeAdapter::new(pinned_version(cfg)))),
        opencode::OpenCodeAdapter::ID => Ok(Arc::new(opencode::OpenCodeAdapter::new(
            pinned_version(cfg),
        ))),
        RETIRED => Err(AdapterError::Protocol(RETIRED_MESSAGE.to_string())),
        other => Err(AdapterError::Protocol(format!(
            "no adapter for harness `{other}`; this node knows {}",
            KNOWN.join(", ")
        ))),
    }
}

/// The version this node pins its harness to. An unset version is not "run
/// whatever is on PATH": it is the version this node's own harness image
/// installs, which is the same string the image build reads. Both are checked
/// against what the harness reports, so an image built elsewhere still fails.
pub fn pinned_version(cfg: &Config) -> String {
    let configured = cfg.harness.version.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    image_version(&cfg.harness.id).to_string()
}

/// Plugin packages the harness image for `harness_id` bakes into its offline
/// package cache. A harness with no plugin mechanism has none, and a name
/// outside this list is refused when a launch manifest is built rather than
/// discovered as a missing module inside a runner with no network.
pub fn baked_plugins(harness_id: &str) -> Vec<String> {
    if harness_id == opencode::OpenCodeAdapter::ID {
        opencode::OpenCodeAdapter::baked_plugins()
    } else {
        Vec::new()
    }
}

/// The version the harness image for `harness_id` installs.
pub fn image_version(harness_id: &str) -> &'static str {
    if harness_id == claude::ClaudeAdapter::ID {
        claude::ClaudeAdapter::PINNED_VERSION
    } else {
        opencode::OpenCodeAdapter::PINNED_VERSION
    }
}

#[derive(Debug, Clone)]
pub struct HarnessVersion {
    pub found: String,
    pub pinned: String,
}

impl HarnessVersion {
    pub fn matches(&self) -> bool {
        self.found == self.pinned
    }
}

/// The wire protocol an adapter drives, and the versions of it it will drive.
///
/// A harness that answers the handshake with a version outside this range is
/// refused rather than driven on the chance that the shapes still line up: the
/// adapter's request and response types were read from one version of one
/// protocol, and a session that proceeds on luck produces a transcript nobody
/// can interpret afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolSupport {
    /// What the protocol is called on the wire: `opencode-http`,
    /// `claude-stream-json`.
    pub name: &'static str,
    pub min: u32,
    pub max: u32,
}

impl ProtocolSupport {
    pub fn accepts(&self, version: u32) -> bool {
        version >= self.min && version <= self.max
    }

    /// The versions, for a message an operator reads: `1` or `1-3`.
    pub fn versions(&self) -> String {
        if self.min == self.max {
            self.min.to_string()
        } else {
            format!("{}-{}", self.min, self.max)
        }
    }

    /// `opencode-http/1` — what a session records as the contract it ran
    /// under.
    pub fn tag(&self, version: u32) -> String {
        format!("{}/{version}", self.name)
    }
}

/// What the harness said about itself in the handshake, as opposed to what the
/// node expected. Recorded on the session row so a transcript read months
/// later can be interpreted against the thing that produced it, and so a
/// mismatch is diagnosable from the row rather than only from a log line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HarnessCompat {
    /// The name the harness calls itself, which need not be the node's id
    /// for it.
    pub agent: String,
    /// The version the harness reported, which the pin was checked against.
    pub version: String,
    /// The protocol and version this session negotiated: `opencode-http/1`.
    pub protocol: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelOption {
    pub value: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool_call_id: Option<String>,
    /// Human-facing summary only. Policy never reads this field.
    pub title: String,
    /// Adapter-normalized action (`read`, `bash`, `todo_write`, ...).
    pub action: String,
    /// Semantic class understood by policy. Unknown or malformed actions have
    /// no kind and therefore cannot match an allow rule by accident.
    pub kind: Option<String>,
    /// Exact non-command target, when the adapter can identify one.
    pub resource: Option<String>,
    /// Exact shell command, only for a recognized execution action.
    pub command: Option<String>,
    pub raw_input: Option<Value>,
    pub options: Vec<crate::adapter::types::PermissionOption>,
}

impl PermissionRequest {
    /// Normalize a managed harness's permission vocabulary into the node's.
    /// The mapping is deliberately closed: a new harness capability is asked
    /// until this node version names its semantics.
    pub fn managed(
        tool_call_id: Option<String>,
        title: String,
        action: &str,
        resource: Option<String>,
        command: Option<String>,
        raw_input: Option<Value>,
        options: Vec<crate::adapter::types::PermissionOption>,
    ) -> Self {
        let action = canonical_action(action);
        let resource = nonempty(resource);
        let command = nonempty(command);
        let kind = match action.as_str() {
            "read" | "glob" | "grep" | "list" if resource.is_some() => Some("read"),
            "bash" if command.is_some() => Some("execute"),
            "edit" | "write" | "patch" | "notebook_edit" if resource.is_some() => Some("write"),
            "think" | "todo_write" => Some("think"),
            _ => None,
        }
        .map(str::to_string);
        Self {
            tool_call_id,
            title,
            action,
            kind,
            resource,
            command,
            raw_input,
            options,
        }
    }
}

fn canonical_action(action: &str) -> String {
    match action.trim().to_ascii_lowercase().as_str() {
        "ls" => "list".into(),
        "todowrite" | "todo_write" => "todo_write".into(),
        "notebookedit" | "notebook_edit" => "notebook_edit".into(),
        other => other.to_string(),
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

/// The operator's answer to a permission request, or the request being withdrawn.
#[derive(Debug)]
pub enum PermissionReply {
    Selected(String),
    /// Allowed, with the arguments the operator rewrote on the card. Only a
    /// brokered tool call is answered this way; a harness's own request never is.
    Edited {
        option_id: String,
        arguments: Value,
    },
    Cancelled,
}

#[derive(Debug)]
pub enum HarnessEvent {
    MessageChunk {
        message_id: Option<String>,
        text: String,
    },
    ThoughtChunk {
        message_id: Option<String>,
        text: String,
    },
    ToolCall(crate::adapter::types::ToolCall),
    ToolCallUpdate(crate::adapter::types::ToolCallUpdate),
    Plan(Value),
    Usage {
        size: Option<u64>,
        used: Option<u64>,
        cost_usd: Option<f64>,
    },
    Permission {
        request: PermissionRequest,
        reply: oneshot::Sender<PermissionReply>,
    },
    Models(Vec<ModelOption>),
    Other(Value),
    Exited {
        code: Option<i32>,
    },
}

#[derive(Debug, Clone)]
pub struct TurnResult {
    pub stop_reason: String,
    pub usage: crate::adapter::types::Usage,
}

pub struct LaunchSpec {
    pub cwd_in_runner: String,
    pub model: String,
    pub container_name: String,
    /// The harness user's home inside the runner. A harness whose state is a
    /// per-session tree rather than a single directory (OpenCode's HOME and
    /// four XDG dirs) needs to know where it may keep that tree; the stdio
    /// harnesses ignore it.
    pub harness_home: String,
    /// MCP servers offered to the harness at session start. The node's own
    /// tools reach it this way and no other.
    pub mcp_servers: Vec<Value>,
    /// Built-in tools the harness may use. Empty leaves its default set.
    pub tools: Vec<String>,
    /// Environment for the harness process: the gateway wiring.
    pub env: Vec<(String, String)>,
    /// A file inside the runner appended to the harness's system prompt: the
    /// session's orientation.
    pub system_prompt_file: Option<String>,
    /// Where the harness's durable event stream resumes from, and what has
    /// already been ingested. `None` leaves the adapter to keep the sequence
    /// in its own memory, which is all a stdio harness can do; a harness with
    /// a durable, sequenced stream is given the node's own cursor so a restart
    /// resumes where the store says rather than where the process forgot.
    pub cursor: Option<Arc<dyn DurableCursor>>,
}

/// The harness's own HTTP surface, narrowed to what reconciliation needs: ask
/// it something, tell it something. The adapter implements this over the
/// client it already holds — the credential, the base address and the pinned
/// directory stay behind it — so the ingestion layer can drive the snapshot
/// and reply routes without owning a second connection to the harness or a
/// second copy of its credential.
#[async_trait]
pub trait HarnessSnapshots: Send + Sync {
    async fn get(&self, path: &str) -> Result<Value, AdapterError>;
    async fn post(&self, path: &str, body: Value) -> Result<Value, AdapterError>;
}

/// The node's ownership of a durable stream's position.
///
/// OpenCode's per-session stream is the only one with replay, and it is
/// anchored on an aggregate sequence (`api-ui.md` §3, finding 6). Which
/// sequence to resume from is a durable fact about the session, not a variable
/// in the adapter's loop: a node that restarts mid-turn has to ask the store,
/// or it either replays a turn it already recorded or loses one it never did.
/// So the adapter asks this, and admits each event through it.
#[async_trait]
pub trait DurableCursor: Send + Sync {
    /// The upstream session the handshake created, and the client to reconcile
    /// against. Called once the adapter knows both, before the stream opens.
    async fn bind(&self, upstream_session_id: &str, api: Arc<dyn HarnessSnapshots>);

    /// The durable sequence to resume from — the `?after=` of the next
    /// connection.
    async fn resume_from(&self) -> u64;

    /// Admit one durable event. `false` means it has already been ingested (a
    /// reconnect overlap, a restart replay, a snapshot that got there first),
    /// and the adapter must not translate it into a harness event a second
    /// time.
    async fn admit(&self, event: &Value) -> bool;

    /// Every event the adapter read, admitted or not, before anything is
    /// decided about it.
    ///
    /// Ingestion is the node's *record*; this is the node's *tap*. The native
    /// UI's only live channel is a stream the node synthesises rather than
    /// forwards (`gateway::native_events`, finding 20), and it is built from
    /// what the adapter is already reading — so the two pumps stay the only
    /// readers of the harness's streams. Deliberately not gated on `admit`: a
    /// replayed event is a duplicate for the record and still the freshest
    /// thing a browser that just reconnected has seen.
    ///
    /// Cheap, synchronous and infallible by contract: it runs on the pump's
    /// own loop, and a tap that could block would stall ingestion.
    fn observe(&self, _event: &Value) {}

    /// The stream dropped and is about to be reopened. What was missed comes
    /// back through the resumed stream itself; this is where anything the
    /// stream cannot carry — a permission raised while it was down, a session
    /// that no longer exists — is reconciled.
    async fn reconnected(&self);
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("runner: {0}")]
    Runner(#[from] crate::runner::RunnerError),
    #[error("harness has no stdio pipe")]
    NoPipe,
    #[error("version mismatch: found {found}, pinned {pinned}")]
    VersionMismatch { found: String, pinned: String },
    #[error("harness {agent} reports {protocol} protocol {found}; this node supports {supported}")]
    IncompatibleProtocol {
        agent: String,
        protocol: &'static str,
        found: String,
        supported: String,
    },
    #[error("model {0:?} is not offered by the harness")]
    UnknownModel(String),
    /// The stored credential cannot be renewed in place; only a fresh login
    /// can replace it. A refresh loop has to stop asking rather than retry a
    /// thing that will never succeed.
    #[error("{0}")]
    ReconnectRequired(String),
    #[error("{0}")]
    Protocol(String),
}

#[async_trait]
pub trait HarnessAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn pinned_version(&self) -> &str;

    /// The protocol this adapter drives, and the versions of it it accepts.
    fn protocol(&self) -> ProtocolSupport;

    /// Where this harness keeps its state, and what it calls that place.
    fn layout(&self) -> Layout {
        layout(self.id())
    }

    /// Files the harness must find in its state directory, as paths relative
    /// to that directory and their contents. This is where a harness's own
    /// configuration lives: the node materializes it per session rather than
    /// writing into the operator's real one.
    fn scratch_files(&self, _wiring: &Wiring) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Directories inside the state directory the harness must be able to
    /// write, as paths relative to it. They are mounted from this session's
    /// own scratch volume, so a harness that keeps per-session state (a home,
    /// XDG dirs, its database) writes it where it dies with the session
    /// rather than into the state volume every session shares.
    fn scratch_dirs(&self) -> Vec<String> {
        Vec::new()
    }

    async fn version(&self, runner: &dyn Runner) -> Result<HarnessVersion, AdapterError>;
    /// The models this node can run through the harness: probed from it where
    /// it keeps a catalogue, declared by the node where it does not. The
    /// wiring is the whole answer for a declaring harness and the environment
    /// for a probing one, so both get it rather than only the environment.
    async fn probe_models(
        &self,
        runner: &dyn Runner,
        wiring: &Wiring,
    ) -> Result<Vec<ModelOption>, AdapterError>;
    async fn launch(
        &self,
        runner: &dyn Runner,
        spec: LaunchSpec,
    ) -> Result<(Box<dyn HarnessHandle>, mpsc::Receiver<HarnessEvent>), AdapterError>;
}

/// A running harness's own HTTP API, as the gateway that mediates it needs to
/// reach it. The credential stays on the node: the gateway injects it upstream
/// and the browser never sees it (`docs/reference/opencode-v1.18.30/`,
/// finding 4).
#[derive(Debug, Clone)]
pub struct NativeApi {
    /// `http://host:port` — this session's server and no other's.
    pub base: String,
    /// The `Authorization` header value that endpoint demands.
    pub authorization: String,
    /// The workspace directory, in the runner's namespace, that every
    /// forwarded request is pinned to.
    pub directory: String,
    /// The harness's own session id: the only one the gateway forwards.
    pub session_id: String,
}

#[async_trait]
pub trait HarnessHandle: Send + Sync {
    fn harness_session_id(&self) -> &str;
    /// The harness's own HTTP API, when it has one worth mediating. A harness
    /// driven over a pipe has none, and the gateway answers for no session it
    /// cannot reach this way.
    fn native_api(&self) -> Option<NativeApi> {
        None
    }
    /// What the handshake that produced this handle reported. Available only
    /// once the handshake succeeded, which is the only time it is true.
    fn compat(&self) -> HarnessCompat;
    async fn prompt(&self, text: String) -> Result<TurnResult, AdapterError>;
    async fn cancel(&self) -> Result<(), AdapterError>;
    /// Establish an adapter-specific ordering barrier after a turn response.
    /// A harness whose events arrive on a queue of their own must drain the
    /// ones that preceded the turn response before the supervisor can resume a
    /// paused session.
    async fn quiesce_events(&self) -> Result<(), AdapterError> {
        Ok(())
    }
    async fn close(&self) -> Result<(), AdapterError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_harness_picks_the_adapter() {
        let mut cfg = Config::default();
        cfg.harness.id = "claude".into();
        let Ok(a) = adapter_for(&cfg) else {
            panic!("claude has an adapter")
        };
        assert_eq!(a.id(), "claude");
        assert_eq!(a.layout().dir, ".claude");
        assert_eq!(a.layout().env, "CLAUDE_CONFIG_DIR");
    }

    /// Falling back to a default would run a harness the operator did not ask
    /// for, under a version pin that does not describe it. Refusing at startup
    /// is the only safe answer.
    #[test]
    fn an_unknown_harness_is_refused_rather_than_guessed_at() {
        let mut cfg = Config::default();
        cfg.harness.id = "not-a-harness".into();
        let err = match adapter_for(&cfg) {
            Ok(_) => panic!("an unknown harness must not resolve"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("not-a-harness"), "{err}");
        assert!(err.contains("opencode"), "{err}");
        assert!(err.contains("claude"), "{err}");
    }

    /// The retired harness is not merely unknown. An operator whose node.toml
    /// still says `omp` gets the migration path — the supported ids, the
    /// archive step, and how to carry one session forward — rather than a
    /// list of adapter names that leaves them to guess what happened to their
    /// sessions.
    #[test]
    fn the_retired_harness_is_refused_with_the_migration_path() {
        let mut cfg = Config::default();
        cfg.harness.id = RETIRED.into();
        let err = match adapter_for(&cfg) {
            Ok(_) => panic!("the retired harness must not resolve"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("was removed"), "{err}");
        assert!(err.contains("archive-legacy"), "{err}");
        assert!(err.contains("session reopen"), "{err}");
        assert!(err.contains("opencode"), "{err}");
    }

    /// The OpenCode adapter drives a server rather than stdio, and its state
    /// is a per-session tree rather than one directory the runner can name.
    #[test]
    fn the_configured_opencode_harness_picks_its_own_adapter() {
        let mut cfg = Config::default();
        cfg.harness.id = "opencode".into();
        cfg.harness.version = String::new();
        let Ok(a) = adapter_for(&cfg) else {
            panic!("opencode has an adapter")
        };
        assert_eq!(a.id(), "opencode");
        assert_eq!(a.layout().dir, ".opencode");
        assert_eq!(a.layout().env, "");
        assert_eq!(
            a.pinned_version(),
            opencode::OpenCodeAdapter::PINNED_VERSION
        );
        assert_eq!(a.protocol().name, "opencode-http");
    }

    /// The pin has to be one fact, not two that happen to agree today: the
    /// node checks what the harness reports against this constant, and the
    /// image build fetches a release by the `ARG` below. If they drift, a node
    /// running its own image refuses every session, and the reason ("found X,
    /// pinned Y") reads like a broken harness rather than a stale constant.
    fn container_arg(containerfile: &str, arg: &str) -> String {
        let prefix = format!("ARG {arg}=");
        containerfile
            .lines()
            .find_map(|line| line.trim().strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("{arg} is set in the Containerfile"))
            .trim()
            .to_string()
    }

    #[test]
    fn the_image_installs_the_pinned_opencode() {
        assert_eq!(
            container_arg(
                include_str!("../../../containers/harness-opencode/Containerfile"),
                "OPENCODE_VERSION"
            ),
            opencode::OpenCodeAdapter::PINNED_VERSION
        );
    }

    #[test]
    fn the_image_installs_the_pinned_claude() {
        assert_eq!(
            container_arg(
                include_str!("../../../containers/harness-claude/Containerfile"),
                "CLAUDE_VERSION"
            ),
            claude::ClaudeAdapter::PINNED_VERSION
        );
    }

    /// An unset version is the image's, not "whatever is on PATH": the check
    /// still runs, against the version this node's own image installs.
    #[test]
    fn an_unset_version_pins_to_what_the_image_installs() {
        let mut cfg = Config::default();
        cfg.harness.version = String::new();
        cfg.harness.id = "claude".into();
        let Ok(a) = adapter_for(&cfg) else {
            panic!("claude has an adapter")
        };
        assert_eq!(a.pinned_version(), claude::ClaudeAdapter::PINNED_VERSION);
        cfg.harness.id = "opencode".into();
        let Ok(a) = adapter_for(&cfg) else {
            panic!("opencode has an adapter")
        };
        assert_eq!(
            a.pinned_version(),
            opencode::OpenCodeAdapter::PINNED_VERSION
        );
    }

    /// Only the two supported images are agreed with. The retired harness had
    /// a third, `containers/harness`, and it is gone: a build that still
    /// carried it would be an image nothing can launch.
    #[test]
    fn only_the_supported_harnesses_have_images() {
        assert_eq!(KNOWN, ["opencode", "claude"]);
        let containers = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../containers");
        assert!(
            !containers.join("harness").exists(),
            "the retired harness's image directory is still here"
        );
        for id in KNOWN {
            assert!(
                containers
                    .join(format!("harness-{id}"))
                    .join("Containerfile")
                    .exists(),
                "{id} has no Containerfile"
            );
        }
    }

    #[test]
    fn unknown_or_malformed_managed_actions_have_no_exempt_kind() {
        let unknown = PermissionRequest::managed(
            None,
            "friendly display title".into(),
            "future-capability",
            Some("/work/file".into()),
            None,
            None,
            Vec::new(),
        );
        assert_eq!(unknown.action, "future-capability");
        assert_eq!(unknown.kind, None);
        assert_eq!(unknown.resource.as_deref(), Some("/work/file"));

        let targetless_read =
            PermissionRequest::managed(None, "read".into(), "read", None, None, None, Vec::new());
        assert_eq!(targetless_read.kind, None);
    }

    #[test]
    fn a_protocol_range_reads_as_one_version_or_a_span() {
        let one = ProtocolSupport {
            name: "opencode-http",
            min: 1,
            max: 1,
        };
        assert!(one.accepts(1));
        assert!(!one.accepts(0));
        assert!(!one.accepts(2));
        assert_eq!(one.versions(), "1");
        assert_eq!(one.tag(1), "opencode-http/1");
        let span = ProtocolSupport {
            name: "opencode-http",
            min: 1,
            max: 3,
        };
        assert!(span.accepts(2));
        assert_eq!(span.versions(), "1-3");
    }
}
