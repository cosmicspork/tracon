//! The harness-agnostic seam. The trait has been here from the first commit
//! because adapters are the part that rots; what a harness is called, where it
//! keeps its state, and what it must find in that state directory all live
//! behind it rather than being spelled with one harness's name throughout the
//! node. That seam is what let the omp harness be removed at the OpenCode
//! cutover, and OpenCode after it, without the rest of the node noticing.

pub mod claude;
pub mod compat;
pub mod native;
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
    /// such variable, and then the runners set none rather than inventing one.
    pub env: &'static str,
}

/// The harness ids every node runs. Claude Code for the Anthropic
/// credentials, and tracon's own harness for every other: without it a model
/// that is not Claude has nothing to run on.
pub const KNOWN: &[&str] = &[claude::ClaudeAdapter::ID, native::NativeAdapter::ID];

/// Harnesses with an adapter that run only where the operator turns them on
/// (`[harness] experimental`). Every harness a node runs needs its image
/// before the boundary passes, so one still being proven must not be able to
/// stop the supported ones. None today.
pub const EXPERIMENTAL: &[&str] = &[];

/// The harnesses this node runs: the supported ones, then the experimental
/// ones it turned on.
pub fn enabled(cfg: &Config) -> Vec<&'static str> {
    KNOWN
        .iter()
        .chain(
            EXPERIMENTAL
                .iter()
                .filter(|id| cfg.harness.experimental.iter().any(|e| e == *id)),
        )
        .copied()
        .collect()
}

pub fn is_enabled(cfg: &Config, harness_id: &str) -> bool {
    enabled(cfg).contains(&harness_id)
}

/// Harnesses this node once ran and no longer has: omp until the 2026-09-13
/// cutover, OpenCode until tracon's own harness replaced it. A `node.toml`
/// that still names one is migrated to Claude Code as it loads
/// (`crate::legacy::migrate_config`); a config that reaches the adapter
/// naming one anyway reads the migration path rather than a bare "no adapter
/// for harness".
pub const RETIRED: &[&str] = &["omp", "opencode"];

pub fn is_retired(harness_id: &str) -> bool {
    RETIRED.contains(&harness_id)
}

/// What that operator is told. The sessions a retired harness ran are not
/// lost — they are archived read-only, keeping their harness identity and
/// version — and the way to carry one forward is a new session under a
/// supported harness, not a relaunch of a harness that is gone.
pub fn retired_message(harness_id: &str) -> String {
    format!(
        "the `{harness_id}` harness was removed from tracon. \
         A node.toml that names it is migrated to \"claude\" when the node loads it; \
         this configuration was not. \
         Set [harness] id = \"claude\" in node.toml, then run `tracon setup` to build \
         the images and `tracon session archive-legacy` to put its sessions away \
         read-only. Carry one forward with \
         `tracon session reopen <id> --harness claude` (or `native`)."
    )
}

/// The layout for a harness id, for the few callers that have the config but
/// not the adapter (the boundary preflight). An id that is not native's gets
/// Claude Code's, which only that preflight can reach: `adapter_for` refuses
/// an unknown id first, so no session ever runs against a layout that is not
/// its harness's.
pub fn layout(harness_id: &str) -> Layout {
    match harness_id {
        native::NativeAdapter::ID => native::NativeAdapter::layout(),
        _ => claude::ClaudeAdapter::layout(),
    }
}

/// The adapter for the configured harness. An unknown id fails here, at
/// startup, rather than silently running whichever harness happens to be the
/// default — a node that thinks it is running something else is worse than a
/// node that will not start.
pub fn adapter_for(cfg: &Config) -> Result<Arc<dyn HarnessAdapter>, AdapterError> {
    adapter_for_id(cfg, &cfg.harness.id)
}

/// The adapter for any supported harness, whether or not it is the configured
/// one. A session names its harness (`session.harness_id`), and the node runs
/// it with this; the configured id is only the default.
pub fn adapter_for_id(
    cfg: &Config,
    harness_id: &str,
) -> Result<Arc<dyn HarnessAdapter>, AdapterError> {
    match harness_id {
        claude::ClaudeAdapter::ID => Ok(Arc::new(claude::ClaudeAdapter::new(pinned_version_for(
            cfg, harness_id,
        )))),
        native::NativeAdapter::ID => Ok(Arc::new(native::NativeAdapter::new(pinned_version_for(
            cfg, harness_id,
        )))),
        retired if is_retired(retired) => Err(AdapterError::Protocol(retired_message(retired))),
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
    pinned_version_for(cfg, &cfg.harness.id)
}

/// One picker list from two harnesses' catalogues, `first` in its own order
/// ahead of what `more` adds. Claude Code names an Anthropic model bare where
/// a provider-qualified list names it `anthropic/<id>`; the bare entry takes
/// the qualified spelling where it stands, since it says which credential it
/// spends, and the duplicate is dropped.
pub fn merge_catalogues(first: Vec<ModelOption>, more: Vec<ModelOption>) -> Vec<ModelOption> {
    let all: Vec<ModelOption> = first.into_iter().chain(more).collect();
    let qualified: std::collections::HashSet<String> = all
        .iter()
        .filter_map(|model| model.value.strip_prefix("anthropic/").map(str::to_string))
        .collect();
    let mut seen = std::collections::HashSet::new();
    all.into_iter()
        .map(|mut model| {
            if !model.value.contains('/') && qualified.contains(&model.value) {
                model.value = format!("anthropic/{}", model.value);
            }
            model
        })
        .filter(|model| seen.insert(model.value.clone()))
        .collect()
}

/// `[harness] version` pins the configured harness only; any other runs at
/// the version its image installs.
pub fn pinned_version_for(cfg: &Config, harness_id: &str) -> String {
    let configured = cfg.harness.version.trim();
    if harness_id == cfg.harness.id && !configured.is_empty() {
        return configured.to_string();
    }
    image_version(harness_id).to_string()
}

/// The version the harness image for `harness_id` installs.
pub fn image_version(harness_id: &str) -> &'static str {
    match harness_id {
        native::NativeAdapter::ID => native::NativeAdapter::PINNED_VERSION,
        _ => claude::ClaudeAdapter::PINNED_VERSION,
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
    /// What the protocol is called on the wire: `claude-stream-json`, `acp`.
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

    /// `claude-stream-json/1` — what a session records as the contract it ran
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
    /// The protocol and version this session negotiated: `claude-stream-json/1`.
    pub protocol: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelOption {
    pub value: String,
    pub name: String,
    /// The harnesses this model's credential runs on, as the node classified
    /// it when it built the catalogue. Empty where it could not say, and from
    /// an older build.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<String>,
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
    /// The call this one was made under: a subagent's call names the
    /// parent's call that spawned it.
    pub parent_tool_call_id: Option<String>,
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
            "edit" | "write" | "patch" | "cell_edit" if resource.is_some() => Some("write"),
            // A subagent's own calls are each asked; spawning one only
            // starts it thinking.
            "think" | "todo_write" | "spawn" => Some("think"),
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
            parent_tool_call_id: None,
        }
    }
}

fn canonical_action(action: &str) -> String {
    let action = action.trim().to_ascii_lowercase();
    match action.as_str() {
        "ls" => "list".into(),
        "todowrite" | "todo_write" => "todo_write".into(),
        action if is_cell_edit_action(action) => "cell_edit".into(),
        _ => action,
    }
}

fn is_cell_edit_action(action: &str) -> bool {
    action.as_bytes() == b"note\x62ookedit" || action.as_bytes() == b"note\x62ook_edit"
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
    /// Refused by the node itself, never shown to the operator: the signed
    /// policy's denial, with the rule and the reason it gives. A harness that
    /// can carry a message tells the agent this, so it does not read a policy
    /// rule as the operator's choice.
    Refused {
        rule: Option<String>,
        reason: Option<String>,
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
    /// One upstream model request as the harness counted it, for matching
    /// against the gateway's row of the same request.
    RequestUsage(crate::adapter::types::RequestUsage),
    Permission {
        request: PermissionRequest,
        reply: oneshot::Sender<PermissionReply>,
    },
    /// A tool call the harness let through on its own permission rules,
    /// never asking the node. It has run (or is running); the node only
    /// records the decision its own policy makes for it.
    Decided(PermissionRequest),
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

#[derive(Clone)]
pub struct LaunchSpec {
    pub cwd_in_runner: String,
    pub model: String,
    pub container_name: String,
    /// The harness user's home inside the runner.
    pub harness_home: String,
    /// MCP servers offered to the harness at session start. The node's own
    /// tools reach it this way and no other.
    pub mcp_servers: Vec<Value>,
    /// Built-in tools the harness may use. Empty leaves the adapter's default
    /// set (for Claude Code, the few built-ins a node session uses).
    pub tools: Vec<String>,
    /// Environment for the harness process: the gateway wiring.
    pub env: Vec<(String, String)>,
    /// Environment for the harness process that must not be readable from
    /// its pod spec or the runtime's argv: the session's egress grant, whose
    /// proxy URL carries a password. See `RunnerCommand::secret_env`.
    pub secret_env: crate::runner::SecretEnv,
    /// A file inside the runner appended to the harness's system prompt: the
    /// session's orientation.
    pub system_prompt_file: Option<String>,
    /// Carry on this harness session instead of starting one: set when the
    /// node relaunches a harness whose process died (`restorable`).
    pub resume: Option<Resume>,
}

/// Where a relaunched harness picks its session up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    pub harness_session_id: String,
    /// The last update sequence the node received, so nothing at or below it
    /// is taken twice.
    pub last_seq: u64,
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

    /// Directories inside the state directory mounted read-only as a whole,
    /// with whatever `scratch_files` stages under them. A file mounted alone
    /// sits in a directory the harness may still write beside it; a harness
    /// that loads every config file it finds in a directory needs the
    /// directory sealed, not the file.
    fn readonly_dirs(&self) -> Vec<String> {
        Vec::new()
    }

    /// Where, relative to the state directory, the harness reads the
    /// orientation as its own standing instructions, if it reads one from a
    /// file it finds rather than one it is handed at launch.
    fn orientation_file(&self) -> Option<String> {
        None
    }

    /// Paths, relative to the workspace root, that this harness would load as
    /// its own configuration. A session whose workspace carries one is
    /// refused: the repository would be configuring the harness the node
    /// configures.
    fn refused_workspace_entries(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether a session survives its harness process: a new process,
    /// launched with `LaunchSpec::resume`, carries the session on from where
    /// the last left it. The supervisor relaunches only a harness that says so.
    fn restorable(&self) -> bool {
        false
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

#[async_trait]
pub trait HarnessHandle: Send + Sync {
    fn harness_session_id(&self) -> &str;
    /// What the handshake that produced this handle reported. Available only
    /// once the handshake succeeded, which is the only time it is true.
    fn compat(&self) -> HarnessCompat;
    /// Where a new process would carry this session on from, for a harness
    /// that is `restorable`.
    fn resume_point(&self) -> Option<Resume> {
        None
    }
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
    #[test]
    fn a_merged_catalogue_keeps_one_spelling_of_each_model() {
        let option = |value: &str| super::ModelOption {
            value: value.into(),
            name: value.into(),
            harnesses: Vec::new(),
        };
        let claude = vec![option("claude-opus-5"), option("claude-sonnet-5")];
        let qualified = vec![
            option("anthropic/claude-opus-5"),
            option("openai-codex/gpt-5.5"),
            option("anthropic/claude-opus-5"),
        ];
        let merged: Vec<String> = super::merge_catalogues(claude, qualified)
            .into_iter()
            .map(|m| m.value)
            .collect();
        assert_eq!(
            merged,
            [
                "anthropic/claude-opus-5",
                "claude-sonnet-5",
                "openai-codex/gpt-5.5"
            ]
        );
    }

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
        assert!(err.contains("claude"), "{err}");
        assert!(err.contains("native"), "{err}");
    }

    /// A retired harness is not merely unknown. An operator whose node.toml
    /// still names one gets the migration path — the supported ids, the
    /// archive step, and how to carry one session forward — rather than a
    /// list of adapter names that leaves them to guess what happened to their
    /// sessions.
    #[test]
    fn a_retired_harness_is_refused_with_the_migration_path() {
        for retired in RETIRED {
            let mut cfg = Config::default();
            cfg.harness.id = (*retired).into();
            let err = match adapter_for(&cfg) {
                Ok(_) => panic!("{retired} must not resolve"),
                Err(e) => e.to_string(),
            };
            assert!(
                err.contains(&format!("`{retired}` harness was removed")),
                "{err}"
            );
            assert!(err.contains("archive-legacy"), "{err}");
            assert!(err.contains("session reopen"), "{err}");
            assert!(err.contains("claude"), "{err}");
        }
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
    fn the_image_installs_the_pinned_claude() {
        assert_eq!(
            container_arg(
                include_str!("../../../containers/harness-claude/Containerfile"),
                "CLAUDE_VERSION"
            ),
            claude::ClaudeAdapter::PINNED_VERSION
        );
    }

    /// The native image fetches release assets by name; the release has to
    /// publish exactly those names, for both architectures a node's Podman
    /// may run.
    #[test]
    fn the_native_image_fetches_what_the_release_publishes() {
        let image = include_str!("../../../containers/harness-native/Containerfile");
        let release = include_str!("../../../.github/workflows/release.yml");
        for triple in ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"] {
            assert!(image.contains(triple), "the image does not fetch {triple}");
            assert!(
                release.contains(&format!("target: {triple}")),
                "the release builds nothing for {triple}"
            );
        }
        assert!(image.contains("tracon-agent-${triple}"));
        assert!(image.contains("ARG TRACON_AGENT_VERSION"));
        assert_eq!(
            native::NativeAdapter::PINNED_VERSION,
            env!("CARGO_PKG_VERSION")
        );
    }

    /// Every model that is not Claude runs on the native harness, so every
    /// node runs it; an `experimental` list naming it changes nothing.
    #[test]
    fn every_node_runs_claude_and_the_native_harness() {
        let mut cfg = Config::default();
        assert_eq!(enabled(&cfg), ["claude", "native"]);
        cfg.harness.experimental = vec!["native".into()];
        assert_eq!(enabled(&cfg), ["claude", "native"]);
        let a = adapter_for_id(&cfg, "native").unwrap();
        assert_eq!(a.pinned_version(), native::NativeAdapter::PINNED_VERSION);
        assert_eq!(layout("native").dir, ".tracon-agent");
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
        cfg.harness.id = "native".into();
        let Ok(a) = adapter_for(&cfg) else {
            panic!("native has an adapter")
        };
        assert_eq!(a.pinned_version(), native::NativeAdapter::PINNED_VERSION);
    }

    /// Only the harnesses with an adapter have images. The retired harnesses
    /// had them, `containers/harness` and `containers/harness-opencode`, and
    /// they are gone: a build that still carried one would be an image
    /// nothing can launch.
    #[test]
    fn only_the_supported_harnesses_have_images() {
        assert_eq!(KNOWN, ["claude", "native"]);
        assert!(EXPERIMENTAL.is_empty());
        let containers = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../containers");
        for retired in ["harness", "harness-opencode"] {
            assert!(
                !containers.join(retired).exists(),
                "the retired {retired} image directory is still here"
            );
        }
        for id in KNOWN.iter().chain(EXPERIMENTAL) {
            assert!(
                containers
                    .join(format!("harness-{id}"))
                    .join("Containerfile")
                    .exists(),
                "{id} has no Containerfile"
            );
        }
    }

    /// Spawning a subagent is thinking, which the shipped policy allows:
    /// what the subagent then does is asked call by call.
    #[test]
    fn a_spawn_is_thinking_and_the_shipped_policy_allows_it() {
        let spawn = PermissionRequest::managed(
            Some("c1".into()),
            "subagent: look".into(),
            "spawn",
            None,
            None,
            None,
            Vec::new(),
        );
        assert_eq!(spawn.kind.as_deref(), Some("think"));
        let decision = crate::policy::Policy::shipped().decide(&crate::policy::Request {
            channel: "personal",
            action: &spawn.action,
            kind: spawn.kind.as_deref(),
            resource: None,
            command: None,
            arguments: None,
            doc_kinds: None,
        });
        assert_eq!(decision.verdict, crate::policy::Verdict::Allow);
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
            name: "claude-stream-json",
            min: 1,
            max: 1,
        };
        assert!(one.accepts(1));
        assert!(!one.accepts(0));
        assert!(!one.accepts(2));
        assert_eq!(one.versions(), "1");
        assert_eq!(one.tag(1), "claude-stream-json/1");
        let span = ProtocolSupport {
            name: "claude-stream-json",
            min: 1,
            max: 3,
        };
        assert!(span.accepts(2));
        assert_eq!(span.versions(), "1-3");
    }
}
