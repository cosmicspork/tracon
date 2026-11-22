//! The tools the node exposes to a harness, over MCP.
//!
//! The transport is the gateway's forward: the harness can reach the node and
//! nothing else, and it carries a token minted per session. With `[external]`
//! set, a harness the operator runs themselves reaches the same tools through
//! the operator door instead, with no session, on a channel. Either
//! way a tool is the only shape a credential ever reaches the harness in — as
//! something it may ask the node to do, never as something it holds.

pub mod approvals;
pub mod aside;
pub mod audit;
pub mod branch;
pub mod checking;
pub mod docs;
pub mod egress;
pub mod github;
pub mod gitlab;
pub mod memory;
mod names;
pub mod operator;
pub mod parity;
pub mod plugin_host;
pub mod review;
pub mod schema;
pub mod services;
pub mod setup;
pub mod show;
pub mod upstream;
pub mod wait;
pub mod work;

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{
    adapter::types::OPTION_ALLOW_ONCE,
    broker::SharedBroker,
    config::Config,
    policy::{Decision, Policy, Request, Verdict},
};

/// The policy kind a brokered tool call is evaluated under. Rules with
/// `kinds = ["tool"]` match the tool's name exactly for allow, and any
/// substring of name-plus-arguments for deny.
pub const TOOL_KIND: &str = "tool";

/// The MCP protocol version the node speaks, and announces to a client that
/// names none.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The revisions this server will serve, newest first. Its whole surface is
/// `initialize`, `tools/list`, `tools/call` and `ping`, which are unchanged
/// across these, so an older client is answered in its own revision rather
/// than being told a newer number it did not ask for. A revision that is not
/// on this list is refused by name: the only client here is a harness the node
/// launched, and one speaking an unknown MCP is a compatibility fault worth
/// seeing rather than a handshake to muddle through.
///
/// `2025-11-25` is what Claude Code 2.1.247 and 2.1.295 announce (observed
/// against the CLI: refusing it is exactly the "failed" MCP server its init
/// frame then reports, and the session never gets its tools).
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] =
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub struct Tools {
    pub broker: SharedBroker,
    pub cfg: Arc<Config>,
    /// Every call is decided here before the broker is touched: the same
    /// bundle that answers the harness's own permission requests answers
    /// what the node will do on its behalf. A denied call returns the rule's
    /// reason; one the policy does not cover is put to the operator.
    pub policy: Arc<parking_lot::RwLock<Policy>>,
    /// One client for the forge and tracker tools. Not proxied: the node
    /// reaches those hosts directly; the harness reaches only the node.
    pub http: reqwest::Client,
    /// Set once the manager exists. Review tools need both, and the manager
    /// needs the tools to decide what a session is offered, so the cycle is
    /// broken here rather than by merging the two.
    pub session: std::sync::OnceLock<SessionAccess>,
    /// The `[[mcp_server]]` entries' listings and the MCP sessions the node
    /// holds with them.
    pub upstream: upstream::Registry,
}

#[derive(Clone)]
pub struct SessionAccess {
    pub store: Arc<crate::store::Store>,
    pub manager: crate::session::Manager,
}

/// What a call knows about who is asking. Channel bindings are enforced here,
/// not in the tool: a tool cannot widen its own reach.
#[derive(Debug, Clone)]
pub struct CallContext {
    pub caller: Caller,
    pub channel: String,
    pub node_id: String,
}

/// Who is calling: a session this node runs, a harness the operator runs
/// themselves through the external door, or the operator's own API.
#[derive(Debug, Clone)]
pub enum Caller {
    Session(String),
    External {
        /// What the harness labels itself, for display. Never authority.
        lane: Option<String>,
        /// How many seconds the harness says its client gives one call, so
        /// a status tool may hold longer than the default (`wait::cap`). A
        /// hint, like the lane.
        wait_budget: Option<u64>,
    },
    /// The operator API on the operator's behalf: a plugin's page, whose
    /// requests reach the plugin as `tracon/ui` ([`Tools::ui_for_operator`]).
    /// It never raises an approval: its calls are allowed by policy or
    /// refused.
    Operator {
        /// The route, which its calls are logged under in the channel's
        /// external log.
        lane: String,
    },
    /// A plugin calling through the socket the node serves its instance
    /// (`crate::plugin::host`), as itself. Like the operator API's, its calls
    /// are allowed by policy or refused, never held.
    Plugin {
        plugin: String,
        /// `plugin:<name>`, which its calls and cards are logged under in
        /// the channel's external log.
        lane: String,
    },
}

impl CallContext {
    pub fn session(
        session_id: impl Into<String>,
        channel: impl Into<String>,
        node_id: impl Into<String>,
    ) -> Self {
        Self {
            caller: Caller::Session(session_id.into()),
            channel: channel.into(),
            node_id: node_id.into(),
        }
    }

    /// The session this node runs for the call, if the caller is one.
    pub fn session_id(&self) -> Option<&str> {
        match &self.caller {
            Caller::Session(id) => Some(id),
            Caller::External { .. } | Caller::Operator { .. } | Caller::Plugin { .. } => None,
        }
    }

    pub fn is_external(&self) -> bool {
        matches!(self.caller, Caller::External { .. })
    }

    pub fn lane(&self) -> Option<&str> {
        match &self.caller {
            Caller::External { lane, .. } => lane.as_deref(),
            Caller::Operator { lane } | Caller::Plugin { lane, .. } => Some(lane),
            Caller::Session(_) => None,
        }
    }

    /// The operator API calling on the operator's behalf, from the route
    /// `lane` names.
    pub fn operator(
        lane: impl Into<String>,
        channel: impl Into<String>,
        node_id: impl Into<String>,
    ) -> Self {
        Self {
            caller: Caller::Operator { lane: lane.into() },
            channel: channel.into(),
            node_id: node_id.into(),
        }
    }

    /// The plugin `plugin`'s instance on `channel`, calling the node.
    pub fn plugin(
        plugin: impl Into<String>,
        channel: impl Into<String>,
        node_id: impl Into<String>,
    ) -> Self {
        let plugin = plugin.into();
        Self {
            caller: Caller::Plugin {
                lane: format!("plugin:{plugin}"),
                plugin,
            },
            channel: channel.into(),
            node_id: node_id.into(),
        }
    }

    pub fn external(
        lane: Option<String>,
        channel: impl Into<String>,
        node_id: impl Into<String>,
    ) -> Self {
        Self {
            caller: Caller::External {
                lane,
                wait_budget: None,
            },
            channel: channel.into(),
            node_id: node_id.into(),
        }
    }

    /// The same external caller, with the per-call budget its harness
    /// declared. A session's calls keep the default cap.
    pub fn with_wait_budget(mut self, budget: Option<u64>) -> Self {
        if let Caller::External { wait_budget, .. } = &mut self.caller {
            *wait_budget = budget;
        }
        self
    }

    /// The per-call budget an external harness declared, if any.
    pub fn wait_budget(&self) -> Option<u64> {
        match &self.caller {
            Caller::External { wait_budget, .. } => *wait_budget,
            Caller::Session(_) | Caller::Operator { .. } | Caller::Plugin { .. } => None,
        }
    }
}

/// The tool a UI request's caller context names: no plugin's tool can be
/// called this, since a tool's name holds no `/`.
pub const UI_TOOL: &str = "tracon/ui";

/// What policy and the `[[plugin]]` entry call a plugin's UI: requests
/// from it are decided as calls to `<plugin>__ui`.
pub const UI_ACTION: &str = "ui";

/// The header a UI request carries its signed caller context in, as
/// base64url (no padding) of its JSON.
pub const UI_CONTEXT_HEADER: &str = "tracon-context";

/// The headers of a plugin's answer that reach its UI; the node sets the
/// rest itself.
pub const UI_ANSWER_HEADERS: &[&str] = &["content-type", "content-disposition", "etag"];

/// The largest answer a plugin may give its UI.
pub const UI_ANSWER_MAX: usize = 32 * 1024 * 1024;

/// A request from a plugin's UI, as the plugin is to receive it.
#[derive(Debug)]
pub struct UiRequest {
    pub method: reqwest::Method,
    /// `/api/...`, with `?` and the query when there is one.
    pub path: String,
    /// The request headers passed on, by lowercase name.
    pub headers: Vec<(String, String)>,
    pub body: axum::body::Bytes,
}

/// Why [`Tools::lent`] lends a call nothing, so it does not run.
#[derive(Debug)]
enum NotLent {
    /// What the call names is not a binding it may be lent.
    Refused(String),
    /// A binding it is lent could not be read from the broker.
    Broker(String),
}

impl From<NotLent> for String {
    fn from(e: NotLent) -> String {
        match e {
            NotLent::Refused(why) | NotLent::Broker(why) => why,
        }
    }
}

/// The plugin's answer to a [`UiRequest`].
#[derive(Debug)]
pub struct UiAnswer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: axum::body::Bytes,
}

/// Why a call through [`Tools::call_for_operator`], or a plugin's through
/// its node socket, did not run.
#[derive(Debug)]
pub enum OperatorCallError {
    /// The node has no sessions, so no plugins.
    Unavailable,
    /// The channel is unknown or archived.
    Request(String),
    /// No plugin of that name is installed with that tool, or none is
    /// active.
    NotInstalled { plugin: String },
    /// The plugin is installed but not offered on the channel.
    NotOffered(String),
    /// The plugin cannot be started, does not list the tool, or the
    /// arguments cannot be signed.
    Unreachable(String),
    /// Refused by a rule or the operator's entry.
    Denied(String),
    /// The call would be asked, and an operator API or plugin call is never
    /// held.
    Asked(String),
    /// The plugin answered with an error, or could not be called.
    Tool(String),
}

impl std::fmt::Display for OperatorCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("plugins are not available on this node"),
            Self::NotInstalled { plugin } => {
                write!(f, "plugin {plugin} is not installed on this node")
            }
            Self::Request(m)
            | Self::NotOffered(m)
            | Self::Unreachable(m)
            | Self::Denied(m)
            | Self::Asked(m)
            | Self::Tool(m) => f.write_str(m),
        }
    }
}

struct GatedCall {
    one_shot: bool,
}

/// A plugin's tool, as a namespaced name names it: the plugin, its own name
/// for the tool, and the build active now.
struct PluginTool {
    plugin: String,
    tool: String,
    manifest: crate::plugin::manifest::Manifest,
    digest: String,
    /// The operator's `[[plugin]]` entry for it, if they wrote one.
    entry: Option<crate::config::PluginEntry>,
}

impl PluginTool {
    /// Whether the operator's Allow on its card needs a preview of the call
    /// to have been shown: unless its `[[plugin]]` entry says
    /// `require_preview = false`, and when there is no entry.
    fn requires_preview(&self) -> bool {
        self.entry.as_ref().is_none_or(|e| e.require_preview)
    }

    /// Whether its manifest marks it `consequential`: asked unless a grant
    /// scoped to the one call allows it, and called with an `operation_id`
    /// the node keeps the outcome under.
    fn consequential(&self) -> bool {
        self.manifest
            .tools
            .iter()
            .any(|t| t.name == self.tool && t.consequential)
    }

    /// The arguments as the plugin is sent them: without the `operation_id`
    /// the node adds to a consequential tool, which is the node's own.
    fn forwarded<'a>(&self, args: &'a Value) -> std::borrow::Cow<'a, Value> {
        match args.as_object() {
            Some(map) if self.consequential() && map.contains_key(OPERATION_ID) => {
                let mut map = map.clone();
                map.remove(OPERATION_ID);
                std::borrow::Cow::Owned(Value::Object(map))
            }
            _ => std::borrow::Cow::Borrowed(args),
        }
    }
}

/// The argument a consequential call is recorded and replayed under.
const OPERATION_ID: &str = "operation_id";

/// What an `operation_id` must match (after surrounding whitespace is
/// trimmed): see [`operation_id`].
const OPERATION_ID_PATTERN: &str = "^[A-Za-z0-9-]{8,}$";

/// The `operation_id` property of every consequential tool's schema, so a
/// caller reads the rule before it is refused by it.
pub(crate) fn operation_id_schema() -> Value {
    json!({
        "type": "string",
        "pattern": OPERATION_ID_PATTERN,
        "description": "A stable id for this one operation: at least 8 ASCII letters, digits or \
                        dashes, nothing else (no dots, underscores or spaces), matching \
                        ^[A-Za-z0-9-]{8,}$. A retry with the same id and arguments returns the \
                        outcome already recorded rather than running it again.",
    })
}

/// `schema` with the `operation_id` a consequential plugin tool is called
/// with: the node's argument, never sent to the plugin.
fn with_operation_id(schema: &Value) -> Value {
    let mut schema = schema.clone();
    if !schema.is_object() {
        return schema;
    }
    if !schema["properties"].is_object() {
        schema["properties"] = json!({});
    }
    schema["properties"][OPERATION_ID] = operation_id_schema();
    let mut required: Vec<Value> = schema["required"].as_array().cloned().unwrap_or_default();
    if !required.iter().any(|r| r == OPERATION_ID) {
        required.push(Value::from(OPERATION_ID));
    }
    schema["required"] = Value::Array(required);
    schema
}

/// What [`Tools::plugin_call_parts`] gathers for one request to a plugin.
struct PluginCallParts {
    lease: crate::plugin::supervisor::Lease,
    credentials: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    caller: crate::plugin::context::CallerId,
    timeout: std::time::Duration,
}

/// The method a plugin's card asks for what a call would do.
const PREVIEW_METHOD: &str = "tracon/preview";

/// How long a card's preview may hold up opening it, at most.
const PREVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The largest preview body a card shows.
const PREVIEW_BODY_MAX: usize = 256 << 10;

/// The largest preview token the node keeps, as the contract bounds it.
const PREVIEW_TOKEN_MAX: usize = 4096;

/// The method a plugin converts text in a format it owns with.
const CONVERT_METHOD: &str = "tracon/format/convert";

/// How long a plugin's `tracon/format/convert` may take.
const CONVERT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The largest text a conversion is taken from or answers.
const CONVERT_MAX: usize = 1 << 20;

/// A plugin build's archive digest, shortened as its install directory
/// names it.
fn short_digest(digest: &str) -> &str {
    digest.get(..16).unwrap_or(digest)
}

/// How long starting and listing one plugin may hold up a `tools/list`.
const PLUGIN_LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The longest description a plugin's tool is offered with.
const PLUGIN_DESCRIPTION: usize = 2000;

enum ActionRecord {
    New(String),
    Replay(Value),
}

/// Who a tool definition is offered to: a session the node runs, in its
/// phase, or a harness the operator runs themselves. Each has one list, and
/// `tools/list` and a call are read against the same one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Session(crate::session::Phase),
    External,
}

impl From<crate::session::Phase> for Audience {
    fn from(phase: crate::session::Phase) -> Self {
        Self::Session(phase)
    }
}

impl Audience {
    /// Whether this audience is offered `name`, before the channel's
    /// credentials narrow it further.
    pub fn offers(self, name: &str) -> bool {
        use crate::session::Phase;
        match self {
            Self::Session(Phase::Plan) => Tools::PLAN_TOOLS.contains(&name),
            Self::Session(Phase::Execute) => !Tools::NOT_EXECUTE.contains(&name),
            Self::Session(Phase::Review) => Tools::REVIEW_TOOLS.contains(&name),
            Self::External => !Tools::NOT_EXTERNAL.contains(&name),
        }
    }

    /// Why a call to a tool this audience is not offered is refused.
    fn refusal(self, name: &str) -> String {
        use crate::session::Phase;
        match self {
            Self::Session(Phase::Review) => format!(
                "{name} is not offered in a review session; give a verdict with {}",
                review::VERDICT
            ),
            Self::Session(phase) => {
                let article = if phase == Phase::Execute { "an" } else { "a" };
                let refusal = format!(
                    "{name} is not offered in {article} {} session",
                    phase.as_str()
                );
                if name == work::WORK_CLOSE {
                    format!(
                        "{refusal}: the operator closes the item a session holds, once its \
                         change is merged; until then it stays open for CI and review"
                    )
                } else if name == review::VERDICT {
                    format!(
                        "{refusal}: only a review session gives a verdict; an execute session \
                         calls {}",
                        review::SUBMIT
                    )
                } else {
                    refusal
                }
            }
            Self::External if name == review::VERDICT => format!(
                "{name} is for a review session the node starts; put your own change up with {}",
                review::SUBMIT
            ),
            Self::External => format!(
                "{name} is not offered to a harness you run yourself: it works on the item a \
                 session holds, and yours holds none"
            ),
        }
    }
}

impl Tools {
    /// The tools a plan session gets: what it needs to read, ask and write its
    /// plan, and to record work it finds. Nothing that closes the item,
    /// submits, sets anything up or writes to a forge or tracker; the reading
    /// forge and Jira tools are here, and the channel's credentials decide
    /// which of those it actually has.
    pub const PLAN_TOOLS: &'static [&'static str] = &[
        memory::RECALL,
        memory::RETAIN,
        memory::AMEND,
        docs::DOC_READ,
        docs::DOC_SEARCH,
        docs::DOC_WRITE,
        docs::DOC_EDIT,
        operator::ASK,
        operator::QUESTION_STATUS,
        operator::NOTIFY,
        work::WORK_READY,
        work::WORK_DISCOVER,
        work::WORK_LIST,
        work::WORK_SHOW,
        github::PR_STATUS,
        github::PR_THREADS,
        github::PR_FOR_BRANCH,
        github::RUN_STATUS,
        github::RUN_LOGS,
        github::RUN_WAIT,
        gitlab::MR_STATUS,
        gitlab::MR_DISCUSSIONS,
        gitlab::MR_FOR_BRANCH,
        gitlab::PIPELINE_STATUS,
        gitlab::PIPELINE_WAIT,
        gitlab::PIPELINE_LIST_BY_SHA,
        gitlab::JOB_TRACE,
        // The jira-review plugin's reads, offered when it is installed.
        "jira-review__search",
        "jira-review__issue",
        "jira-review__transitions",
    ];

    /// What an execute session is not offered: everything else is its to use.
    /// A verdict is a review session's account of someone else's change, and
    /// closing the item is the operator's call: the item stays open through
    /// CI and review until they close it.
    pub const NOT_EXECUTE: &'static [&'static str] = &[review::VERDICT, work::WORK_CLOSE];

    /// The tools a review session gets: what it needs to read, what it needs
    /// to exercise the candidate and show what it saw, and its verdict.
    /// Nothing that publishes. The one forge write is drafting its review's
    /// own change when it finds something blocking (`pr_ready`, `mr_ready`,
    /// refused for any other change), which blocks nothing that cannot be
    /// undone.
    pub const REVIEW_TOOLS: &'static [&'static str] = &[
        memory::RECALL,
        docs::DOC_READ,
        docs::DOC_SEARCH,
        // The item under review and its neighbours, read-only.
        work::WORK_LIST,
        work::WORK_SHOW,
        // Running the candidate: a browser in the session's sandbox, decided by
        // the policy bundle by name, and what it saw shown to the operator,
        // bound to the candidate.
        services::START,
        services::STATUS,
        show::SHOW,
        review::VERDICT,
        github::PR_READY,
        gitlab::MR_READY,
    ];

    /// What a session of each phase calls on most turns, marked
    /// `anthropic/alwaysLoad` so a harness that defers MCP tools behind a
    /// search (Claude Code, `adapter::claude`) keeps these loaded: each
    /// deferred tool costs a round trip the first time it is called, and these
    /// are the ones a session cannot do its job without. Everything else is
    /// named to the model and loaded when it asks. Kept short: every one is
    /// sent whole on every request.
    pub fn always_loaded(phase: crate::session::Phase) -> &'static [&'static str] {
        use crate::session::Phase;
        match phase {
            Phase::Plan => &[
                docs::DOC_WRITE,
                docs::DOC_EDIT,
                docs::DOC_READ,
                memory::RECALL,
                operator::ASK,
            ],
            Phase::Execute => &[review::SUBMIT, review::STATUS, show::SHOW, operator::ASK],
            Phase::Review => &[review::VERDICT, show::SHOW],
        }
    }

    /// What an external harness is not offered: a verdict is what a review
    /// session the node started gives on someone else's change. Nor does it
    /// run behind the node's proxy, beside the node's services, in a
    /// workspace the node can fetch into, or in a repository the node can set
    /// up: those tools refuse it, so it is not shown them.
    pub const NOT_EXTERNAL: &'static [&'static str] = &[
        review::VERDICT,
        branch::FETCH,
        egress::REQUEST,
        services::START,
        services::STATUS,
        setup::DRAFT,
        setup::TRY,
        setup::PROPOSE,
    ];

    /// Tool definitions for a channel, narrowed to what `audience` is offered.
    /// This is the one list: `tools/list`, the orientation, the session's
    /// wiring and its explanation all read it.
    pub fn list_for(
        &self,
        channel: &str,
        node_id: &str,
        audience: impl Into<Audience>,
    ) -> Vec<Value> {
        let audience = audience.into();
        self.list(channel, node_id)
            .into_iter()
            .filter(|t| t["name"].as_str().is_some_and(|n| audience.offers(n)))
            .map(|mut t| {
                aside::shape(&mut t, audience);
                t
            })
            .collect()
    }

    /// Tool definitions for a channel. A channel with no credential bound to it
    /// is offered no tools rather than tools that will fail.
    pub fn list(&self, channel: &str, node_id: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let broker = self.broker.read().unwrap();
        let available = broker.available_to(channel, node_id);
        if available.contains(&gitlab::CREDENTIAL) {
            out.extend(gitlab::definitions());
        }
        if available.contains(&github::CREDENTIAL) {
            out.extend(github::definitions());
        }
        // Review tools need no credential of their own: submitting is always
        // allowed, and publishing is what needs one. An agent that cannot
        // publish can still ask for review and be told why it stopped there.
        if self.session.get().is_some() {
            out.extend(review::definitions());
            // Memory and documents need no credential either: the corpus is
            // the node's own, and reading it is what every session starts by
            // doing. This means every session gets an MCP server.
            out.extend(memory::definitions());
            out.extend(docs::definitions(&self.cfg.live().docs.kind));
            // These are intervention-only: asking, pinging, and drafting a
            // report do not touch a credential or widen a tool policy.
            out.extend(operator::definitions());
            // Asking for a host only asks: the operator's answer is what
            // opens anything, and it opens only this session's own grant.
            out.extend(egress::definitions());
            // Showing work reaches the operator and nothing else: it is read
            // from the node's own snapshot and never leaves tracon.
            out.extend(show::definitions());
            // Fetching a branch reads the forge with the repository's own
            // credential and moves one remote-tracking ref of this session's
            // workspace: nothing on the forge changes.
            out.extend(branch::definitions());
            // Drafting and trying a repository's entry reach nothing this
            // session cannot; proposing one is always the operator's call.
            out.extend(setup::definitions());
            out.extend(work::definitions());
            out.extend(approvals::definitions());
            // Only what the operator's catalogue holds, and only by name:
            // the policy bundle decides each service on its own.
            if !self.cfg.service.is_empty() {
                out.extend(services::definitions(&self.cfg));
            }
            // Another server's tools are only ever asked, run or refused
            // here, so they need a node that can hold an approval.
            out.extend(
                self.upstream
                    .definitions(&self.cfg.live().mcp_server, channel, &available),
            );
            // So are a plugin's: what it lists once started, for a channel
            // that binds a credential it declares.
            out.extend(self.plugin_definitions(&available));
        }
        out
    }

    /// The tools of every active plugin offered to a channel that may use
    /// `available`, as their builds last listed them. A plugin not started
    /// yet, or refused at its start, offers nothing.
    fn plugin_definitions(&self, available: &[&str]) -> Vec<Value> {
        let Some(access) = self.session.get() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (name, build) in self.active_plugins(available) {
            let Some(listed) = access
                .manager
                .plugins()
                .listed(&name)
                .filter(|l| l.digest == build.digest)
            else {
                continue;
            };
            for tool in listed.tools.iter().filter(|t| !t.name.is_empty()) {
                let description = if tool.description.trim().is_empty() {
                    format!("{} on plugin {name}.", tool.remote)
                } else {
                    format!("[plugin {name}] {}", tool.description.trim())
                };
                let consequential = build
                    .manifest
                    .tools
                    .iter()
                    .any(|t| t.name == tool.remote && t.consequential);
                let schema = if consequential {
                    with_operation_id(&tool.input_schema)
                } else {
                    tool.input_schema.clone()
                };
                out.push(json!({
                    "name": tool.name,
                    "description": upstream::bounded(&description, PLUGIN_DESCRIPTION),
                    "inputSchema": schema,
                }));
            }
        }
        out
    }

    /// Every installed plugin's active build, by name, offered to a channel
    /// that may use `available`.
    fn active_plugins(&self, available: &[&str]) -> Vec<(String, crate::store::plugins::Build)> {
        let Some(access) = self.session.get() else {
            return Vec::new();
        };
        access
            .store
            .plugins_list()
            .unwrap_or_default()
            .into_iter()
            .filter(|row| row.state != crate::store::plugins::REMOVED)
            .filter_map(|row| Some((row.name, row.active?)))
            .filter(|(_, build)| crate::plugin::offered(&build.manifest, available))
            .collect()
    }

    /// Start each plugin offered to `channel` that is not running, so a
    /// `tools/list` offers what it lists. A plugin that cannot start within
    /// the wait offers what it last listed, if that is its active build's.
    pub async fn refresh_plugins(&self, channel: &str, node_id: &str) {
        let Some(access) = self.session.get() else {
            return;
        };
        let names: Vec<String> = {
            let broker = self.broker.read().unwrap();
            let available = broker.available_to(channel, node_id);
            self.active_plugins(&available)
                .into_iter()
                .map(|(name, _)| name)
                .collect()
        };
        let plugins = access.manager.plugins();
        let starts = names.iter().map(|name| async move {
            match tokio::time::timeout(PLUGIN_LIST_TIMEOUT, plugins.ensure(name, channel)).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(plugin = %name, %channel, %error, "a plugin could not be started to list its tools")
                }
                Err(_) => {
                    tracing::warn!(plugin = %name, %channel, "a plugin did not start within the listing's wait")
                }
            }
        });
        futures_util::future::join_all(starts).await;
    }

    /// The plugin tool a namespaced name names, in the build active now.
    fn plugin_tool(&self, name: &str) -> Option<PluginTool> {
        let access = self.session.get()?;
        let (prefix, rest) = name.split_once(upstream::SEPARATOR)?;
        let row = access.store.plugin_get(prefix).ok()??;
        if row.state == crate::store::plugins::REMOVED {
            return None;
        }
        let build = row.active?;
        build.manifest.tools.iter().find(|t| t.name == rest)?;
        Some(PluginTool {
            plugin: row.name,
            tool: rest.to_string(),
            digest: build.digest,
            manifest: build.manifest,
            entry: self
                .cfg
                .live()
                .plugin
                .iter()
                .find(|p| p.name == prefix)
                .cloned(),
        })
    }

    /// Whether a call to a plugin's tool can be made for this caller at all:
    /// the plugin offers its channel the tool, starts, and listed it, and
    /// the arguments can be signed. Checked before the call is decided, so a
    /// call that could never run never becomes an approval.
    async fn plugin_ready(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
    ) -> Result<(), String> {
        let Some(access) = self.session.get() else {
            return Ok(());
        };
        let Some((prefix, rest)) = name.split_once(upstream::SEPARATOR) else {
            return Ok(());
        };
        let Some(row) = access
            .store
            .plugin_get(prefix)
            .ok()
            .flatten()
            .filter(|r| r.state != crate::store::plugins::REMOVED)
        else {
            return Ok(());
        };
        let Some(build) = row.active else {
            return Err(format!("plugin {prefix} is not installed yet"));
        };
        if !build.manifest.tools.iter().any(|t| t.name == rest) {
            return Err(format!("plugin {prefix} declares no tool {rest}"));
        }
        self.plugin_offered(ctx, prefix, &build.manifest)?;
        self.plugin_reachable(ctx, name, args).await
    }

    /// Whether the plugin `manifest` describes is offered on the caller's
    /// channel: the channel binds a credential it declares, or it declares
    /// none.
    fn plugin_offered(
        &self,
        ctx: &CallContext,
        plugin: &str,
        manifest: &crate::plugin::manifest::Manifest,
    ) -> Result<(), String> {
        let offered = {
            let broker = self.broker.read().unwrap();
            crate::plugin::offered(manifest, &broker.available_to(&ctx.channel, &ctx.node_id))
        };
        if offered {
            return Ok(());
        }
        let declared: Vec<&str> = manifest
            .credentials
            .iter()
            .map(|c| c.pattern.as_str())
            .collect();
        Err(format!(
            "plugin {plugin} is not offered on channel {}: it needs a credential matching {} bound to it",
            ctx.channel,
            declared.join(" or ")
        ))
    }

    /// Whether an offered plugin tool's call can be made: its arguments can
    /// be signed, and the plugin starts and lists the tool.
    async fn plugin_reachable(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
    ) -> Result<(), String> {
        let access = self
            .session
            .get()
            .ok_or("plugins are not available on this node")?;
        let (prefix, rest) = name
            .split_once(upstream::SEPARATOR)
            .ok_or_else(|| format!("{name} names no plugin's tool"))?;
        crate::plugin::context::canonical(args).map_err(|e| e.to_string())?;
        let endpoint = access
            .manager
            .plugins()
            .ensure(prefix, &ctx.channel)
            .await
            .map_err(|e| format!("plugin {prefix} cannot be reached for {name}: {e}"))?;
        if !endpoint.tools.iter().any(|t| t.remote == rest) {
            return Err(format!("plugin {prefix} lists no tool {rest}"));
        }
        Ok(())
    }

    /// What a request to a plugin's tool for `ctx` is made with: its
    /// instance, held while the request runs; the credentials its running
    /// build declares that the channel binds and `args` select ([`Self::lent`]),
    /// one lent only on approval only to a call run from `approval`; who is
    /// calling; and how long it may take. A call run from an approval is
    /// refused if the build it was asked on is no longer the one running,
    /// and any call whose arguments name a binding it cannot be lent.
    async fn plugin_call_parts(
        &self,
        ctx: &CallContext,
        pt: &PluginTool,
        args: &Value,
        approval: Option<&str>,
    ) -> Result<PluginCallParts, String> {
        use crate::plugin::context;
        let access = self
            .session
            .get()
            .ok_or("plugins are not available on this node")?;
        let supervisor = access.manager.plugins();
        let lease = supervisor
            .lease(&pt.plugin, &ctx.channel)
            .await
            .map_err(|e| format!("plugin {} cannot be reached: {e}", pt.plugin))?;
        let endpoint = &lease.endpoint;
        if !endpoint.tools.iter().any(|t| t.remote == pt.tool) {
            return Err(format!("plugin {} lists no tool {}", pt.plugin, pt.tool));
        }
        // An approved call runs only on the build its card showed: an update
        // or rollback since may do something else and be lent other
        // credentials. A call asked before builds were recorded has none.
        if let Some(id) = approval {
            let asked_on = access
                .store
                .approval_plugin_digest(id)
                .map_err(|e| e.to_string())?;
            if let Some(asked_on) = asked_on.filter(|d| *d != endpoint.digest) {
                return Err(format!(
                    "nothing ran: plugin {} changed build after this call was asked (the card \
                     showed build {}, and {} {} is active now); make the call again to be asked \
                     about it on this build",
                    pt.plugin,
                    short_digest(&asked_on),
                    pt.plugin,
                    endpoint.manifest.version,
                ));
            }
        }
        let credentials = self
            .lent(ctx, &endpoint.manifest, args, approval.is_some())
            .map_err(String::from)?;
        let caller = match &ctx.caller {
            Caller::Session(id) => context::CallerId {
                kind: "agent",
                session: Some(id.clone()),
                harness: access
                    .store
                    .get_session(id)
                    .ok()
                    .flatten()
                    .map(|s| s.harness_id),
                plugin: None,
            },
            Caller::External { .. } => context::CallerId {
                kind: "agent",
                session: None,
                harness: Some(crate::session::external::HARNESS_ID.to_string()),
                plugin: None,
            },
            Caller::Operator { .. } => context::CallerId {
                kind: "operator",
                session: None,
                harness: None,
                plugin: None,
            },
            Caller::Plugin { plugin, .. } => context::CallerId {
                kind: "plugin",
                session: None,
                harness: None,
                plugin: Some(plugin.clone()),
            },
        };
        let timeout = std::time::Duration::from_secs(
            pt.entry
                .as_ref()
                .map(crate::config::PluginEntry::timeout_secs)
                .unwrap_or(crate::config::PluginEntry::DEFAULT_TIMEOUT_SECS),
        );
        Ok(PluginCallParts {
            lease,
            credentials,
            caller,
            timeout,
        })
    }

    /// Call a plugin's tool that is allowed, by policy or by the operator
    /// (`approval`), holding its instance while the call runs. It is lent
    /// the credentials its running build declares that the channel binds,
    /// in its signed caller context; one lent only on approval is lent only
    /// to an approved call, which also carries the token of the preview its
    /// card showed, for the plugin to apply against.
    async fn call_plugin(
        &self,
        ctx: &CallContext,
        pt: &PluginTool,
        args: &Value,
        approval: Option<&str>,
    ) -> Result<Value, String> {
        use crate::plugin::context;
        let access = self
            .session
            .get()
            .ok_or("plugins are not available on this node")?;
        let parts = self.plugin_call_parts(ctx, pt, args, approval).await?;
        let preview_token = match approval {
            Some(id) => access.store.preview_token(id).map_err(|e| e.to_string())?,
            None => None,
        };
        let supervisor = access.manager.plugins();
        let endpoint = &parts.lease.endpoint;
        let args = pt.forwarded(args);
        let args = args.as_ref();
        let draft = context::Draft {
            plugin: &pt.plugin,
            token: &endpoint.token,
            tool: &pt.tool,
            arguments: args,
            caller: &parts.caller,
            channel: &ctx.channel,
            approval_id: approval,
            preview_token: preview_token.as_deref(),
            credentials: &parts.credentials,
        };
        self.upstream
            .call_plugin(
                endpoint,
                &pt.plugin,
                &ctx.channel,
                &pt.tool,
                args,
                || context::build(&draft, crate::store::now_ms(), |b| supervisor.sign(b)),
                parts.timeout,
            )
            .await
    }

    /// What a call to a build with `manifest` is lent, by binding: what
    /// [`crate::plugin::lending::lending`] gives the channel's bindings, for
    /// `args` and whether the call is `approved`, the same function the
    /// card's "Lent on allow" reads. A call whose arguments name a binding
    /// it cannot be lent is refused here, before anything is sent.
    fn lent(
        &self,
        ctx: &CallContext,
        manifest: &crate::plugin::manifest::Manifest,
        args: &Value,
        approved: bool,
    ) -> Result<
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
        NotLent,
    > {
        use crate::plugin::lending;
        let broker = self.broker.read().unwrap();
        let available = broker.available_to(&ctx.channel, &ctx.node_id);
        let lent = lending::lending(manifest, &available, args, approved);
        if let Some(why) = lending::refusal(&lent) {
            return Err(NotLent::Refused(format!(
                "nothing ran: plugin {} cannot be lent what this call names on channel {}: {why}",
                manifest.name, ctx.channel
            )));
        }
        let mut credentials = std::collections::BTreeMap::new();
        for binding in lent.iter().flat_map(|l| &l.bindings) {
            let env = broker
                .env_for(binding, &ctx.channel, &ctx.node_id)
                .map_err(|e| NotLent::Broker(e.to_string()))?;
            credentials.insert(binding.clone(), env);
        }
        Ok(credentials)
    }

    /// Call a plugin's tool for another plugin ([`Caller::Plugin`]), through
    /// the socket the node serves the caller: decided by the signed policy
    /// and the callee's `[[plugin]]` entry as an agent's call is, but never
    /// held. A call either would ask about is refused, naming how to allow
    /// it. Every call the channel can take is logged to its external log
    /// under the caller's lane, refusals included.
    pub async fn call_for_operator(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
    ) -> Result<Value, OperatorCallError> {
        let access = self.session.get().ok_or(OperatorCallError::Unavailable)?;
        // The external Stop fences harnesses, not the operator's own API.
        access
            .manager
            .channel_usable(&ctx.channel)
            .map_err(|e| OperatorCallError::Request(e.to_string()))?;
        access.manager.record_for(
            ctx,
            crate::session::state::event_kind::TOOL_CALL,
            None,
            json!({ "title": summarize(name, args), "kind": TOOL_KIND }),
        );
        let mut gate = audit::Gate::default();
        let result = self.operator_gated(ctx, name, args, &mut gate).await;
        if gate.decision == audit::REFUSED && gate.reason.is_none() {
            gate.reason = result.as_ref().err().map(ToString::to_string);
        }
        let outcome = result
            .as_ref()
            .map(|_| Value::Null)
            .map_err(ToString::to_string);
        access.manager.record_for(
            ctx,
            crate::session::state::event_kind::TOOL_DECIDED,
            None,
            audit::record(name, args, &gate, &outcome),
        );
        result
    }

    /// [`Self::call_for_operator`], saying in `gate` how it was decided.
    async fn operator_gated(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
        gate: &mut audit::Gate,
    ) -> Result<Value, OperatorCallError> {
        let Some(pt) = self.plugin_tool(name) else {
            let plugin = name
                .split_once(upstream::SEPARATOR)
                .map_or(name, |(prefix, _)| prefix);
            return Err(OperatorCallError::NotInstalled {
                plugin: plugin.to_string(),
            });
        };
        self.plugin_offered(ctx, &pt.plugin, &pt.manifest)
            .map_err(OperatorCallError::NotOffered)?;
        self.plugin_reachable(ctx, name, args)
            .await
            .map_err(OperatorCallError::Unreachable)?;
        let decision = self.decide(ctx, name, args);
        let decided_by = self.decided_by(&decision);
        match decision.verdict {
            Verdict::Allow => {
                *gate = audit::Gate {
                    decision: audit::POLICY_ALLOWED,
                    decided_by,
                    rule: decision.rule_id.clone(),
                    reason: decision.reason.clone(),
                    approval_id: None,
                };
            }
            Verdict::Deny => {
                *gate = audit::Gate {
                    decision: audit::DENIED,
                    decided_by,
                    rule: decision.rule_id.clone(),
                    reason: decision.reason.clone(),
                    approval_id: None,
                };
                return Err(OperatorCallError::Denied(refusal(decision)));
            }
            Verdict::Ask => {
                let why = decision
                    .reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default();
                let from = match &ctx.caller {
                    Caller::Plugin { plugin, .. } => format!("plugin {plugin}"),
                    _ => "the operator API".to_string(),
                };
                let refused = format!(
                    "{name} would be put to the operator{why}, and a call from {from} \
                     is never held: allow it with [[plugin]] name = \"{}\" and allow = [\"{}\"] \
                     in node.toml, or with a policy bundle that allows it",
                    pt.plugin, pt.tool
                );
                *gate = audit::Gate {
                    decided_by,
                    rule: decision.rule_id.clone(),
                    reason: Some(refused.clone()),
                    ..Default::default()
                };
                return Err(OperatorCallError::Asked(refused));
            }
        }
        self.call_plugin(ctx, &pt, args, None)
            .await
            .map_err(OperatorCallError::Tool)
    }

    /// What a waiting call to a plugin's tool would do, in the plugin's own
    /// words: `tracon/preview`, asked when its card opens, as an asked call
    /// would be made (no approval, nothing lent only on approval). The
    /// token it returns is kept with the approval for the approved dispatch
    /// to carry, and only its hash reaches the page. A plugin that has no
    /// previews leaves the generic card (`None`); one that refuses is shown
    /// refusing, and the call can still be decided.
    pub async fn approval_preview(
        &self,
        approval: &crate::store::approvals::ApprovalRow,
        args: &Value,
    ) -> Option<Value> {
        let access = self.session.get()?;
        if approval.state != crate::store::approvals::PENDING
            || approval.node_id != access.manager.node_id()
        {
            return None;
        }
        let pt = self.plugin_tool(&approval.tool)?;
        // What Allow is held to: a preview shown of exactly this call.
        let shown_for = crate::store::approvals::arguments_hash(&args.to_string());
        let keep = |token: Option<&str>, shown: Option<&Value>| {
            let preview = shown.map(Value::to_string);
            let shown = preview
                .as_deref()
                .map(|preview| crate::store::approvals::Shown {
                    for_arguments: &shown_for,
                    preview,
                });
            if let Err(error) = access.store.set_preview(&approval.id, token, shown) {
                tracing::warn!(approval = %approval.id, %error, "a preview's token was not kept");
            }
        };
        // Unless the operator opted the plugin out, a call it offered no
        // preview of cannot be allowed, and the card says why.
        let blocked = pt
            .requires_preview()
            .then(|| no_preview_offered(&pt.plugin));
        let refused = |error: String| {
            tracing::info!(approval = %approval.id, plugin = %pt.plugin, %error, "a plugin refused a preview");
            let mut out = json!({ "preview": { "status": "refused", "error": error } });
            if let Some(why) = &blocked {
                out["allow_blocked"] = json!(why);
            }
            out
        };
        let result = match self.ask_preview(access, approval, &pt, args).await {
            Ok(Some(result)) => result,
            Ok(None) => {
                keep(None, None);
                return blocked.map(|why| json!({ "allow_blocked": why }));
            }
            Err(error) => {
                keep(None, None);
                return Some(refused(error));
            }
        };
        let label = format!("plugin {}", pt.plugin);
        match preview_shape(&result) {
            Ok(preview) => {
                // What the operator reads, kept with the approval so its
                // decided page still shows it (`preview_shown`).
                let shown = json!({
                    "previewable": preview.previewable,
                    "summary": preview.summary,
                    "body": preview.body,
                });
                keep(preview.token, Some(&shown));
                // The plugin's own one line titles the card, behind the call
                // as the node reads it, once it is of the call as asked
                // rather than an operator's edit of it.
                if shown_for == approval.arguments_hash() {
                    let asked = serde_json::from_str(&approval.arguments).unwrap_or(Value::Null);
                    if let Some(title) =
                        preview_title(&pt.plugin, &pt.tool, &asked, preview.summary)
                    {
                        match access.store.set_approval_title(&approval.id, &title) {
                            Ok(true) => access.manager.publish_queue().await,
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(approval = %approval.id, %error, "a preview's title was not kept")
                            }
                        }
                    }
                }
                let mut out = shown;
                out["status"] = json!("shown");
                out["token_sha256"] = json!(preview.token.map(preview_hash));
                Some(json!({ "preview": out }))
            }
            Err(error) => {
                keep(None, None);
                Some(refused(format!(
                    "{label} answered a preview the contract does not allow: {error}"
                )))
            }
        }
    }

    /// Ask the plugin behind `pt` for `tracon/preview` of `args` as
    /// `approval`'s call would be made: what it answered, `None` when it
    /// offers no previews, or why it gave none. Nothing is kept.
    async fn ask_preview(
        &self,
        access: &SessionAccess,
        approval: &crate::store::approvals::ApprovalRow,
        pt: &PluginTool,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        use crate::plugin::context;
        let ctx = self.approval_call_context(access, approval);
        let parts = self.plugin_call_parts(&ctx, pt, args, None).await?;
        let supervisor = access.manager.plugins();
        let endpoint = &parts.lease.endpoint;
        let args = pt.forwarded(args);
        let args = args.as_ref();
        let draft = context::Draft {
            plugin: &pt.plugin,
            token: &endpoint.token,
            tool: &pt.tool,
            arguments: args,
            caller: &parts.caller,
            channel: &approval.channel,
            approval_id: None,
            preview_token: None,
            credentials: &parts.credentials,
        };
        let label = format!("plugin {}", pt.plugin);
        let answer = self
            .upstream
            .plugin_request(
                endpoint,
                &pt.plugin,
                &approval.channel,
                PREVIEW_METHOD,
                || {
                    let meta =
                        context::build(&draft, crate::store::now_ms(), |b| supervisor.sign(b))
                            .map_err(upstream::Failure::NotSent)?;
                    Ok(json!({
                        "name": pt.tool,
                        "arguments": args,
                        "_meta": { "tracon": meta },
                    }))
                },
                parts.timeout.min(PREVIEW_TIMEOUT),
            )
            .await;
        match answer {
            Ok(result) => Ok(Some(result)),
            Err(upstream::Failure::Rpc { code, .. }) if code == upstream::METHOD_NOT_FOUND => {
                Ok(None)
            }
            Err(failure) => Err(failure.describe(&label)),
        }
    }

    /// A request from a plugin's UI (`crate::http::plugin_ui`), sent to the
    /// plugin's instance for `ctx`'s channel as the operator's, with a signed
    /// caller context in its `tracon-context` header ([`UI_TOOL`]).
    ///
    /// It is decided as a call to `<plugin>__ui`: a signed policy rule or
    /// the operator's `[[plugin]]` entry denying `ui` refuses it, and
    /// otherwise it runs, since the operator opened the UI through the
    /// operator API and is the one asking. It is never held. A request that
    /// changes something (any method but GET and HEAD) is logged to the
    /// channel's external log under `ctx`'s lane, as is any refusal; a read
    /// is not, so a page's polling does not fill the log.
    pub async fn ui_for_operator(
        &self,
        ctx: &CallContext,
        plugin: &str,
        request: UiRequest,
    ) -> Result<UiAnswer, OperatorCallError> {
        let access = self.session.get().ok_or(OperatorCallError::Unavailable)?;
        access
            .manager
            .channel_usable(&ctx.channel)
            .map_err(|e| OperatorCallError::Request(e.to_string()))?;
        let name = format!("{plugin}{}{UI_ACTION}", upstream::SEPARATOR);
        let args = json!({
            "method": request.method.as_str(),
            "path": request.path,
            "body_sha256": hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&request.body)),
        });
        let changes = !matches!(request.method, reqwest::Method::GET | reqwest::Method::HEAD);
        let mut gate = audit::Gate::default();
        let result = self.ui_gated(ctx, plugin, &args, request, &mut gate).await;
        let refused = matches!(gate.decision, audit::DENIED | audit::REFUSED);
        if changes || refused {
            if gate.decision == audit::REFUSED && gate.reason.is_none() {
                gate.reason = result.as_ref().err().map(ToString::to_string);
            }
            access.manager.record_for(
                ctx,
                crate::session::state::event_kind::TOOL_CALL,
                None,
                json!({ "title": summarize(&name, &args), "kind": TOOL_KIND }),
            );
            let outcome = match &result {
                Ok(answer) => Ok(json!({ "status": answer.status })),
                Err(e) => Err(e.to_string()),
            };
            access.manager.record_for(
                ctx,
                crate::session::state::event_kind::TOOL_DECIDED,
                None,
                audit::record(&name, &args, &gate, &outcome),
            );
        }
        result
    }

    async fn ui_gated(
        &self,
        ctx: &CallContext,
        plugin: &str,
        args: &Value,
        request: UiRequest,
        gate: &mut audit::Gate,
    ) -> Result<UiAnswer, OperatorCallError> {
        use crate::plugin::context;
        let access = self.session.get().ok_or(OperatorCallError::Unavailable)?;
        let refuse = |gate: &mut audit::Gate, e: OperatorCallError| {
            gate.decision = audit::REFUSED;
            gate.decided_by = "node";
            Err(e)
        };
        let build = access
            .store
            .plugin_get(plugin)
            .ok()
            .flatten()
            .filter(|r| r.state != crate::store::plugins::REMOVED)
            .and_then(|r| r.active)
            .filter(|b| b.manifest.ui.is_some());
        let Some(build) = build else {
            return refuse(
                gate,
                OperatorCallError::NotInstalled {
                    plugin: plugin.to_string(),
                },
            );
        };
        if let Err(e) = self.plugin_offered(ctx, plugin, &build.manifest) {
            return refuse(gate, OperatorCallError::NotOffered(e));
        }
        let name = format!("{plugin}{}{UI_ACTION}", upstream::SEPARATOR);
        let live = self.cfg.live();
        let decision = self.policy.read().decide(&Request {
            channel: &ctx.channel,
            action: &name,
            kind: Some(TOOL_KIND),
            resource: None,
            command: None,
            arguments: Some(args),
            doc_kinds: Some(&live.docs.kind),
        });
        let decided_by = self.decided_by(&decision);
        let entry = live.plugin.iter().find(|p| p.name == plugin);
        let denied_by_entry = entry.is_some_and(|e| {
            e.deny
                .iter()
                .any(|pattern| crate::policy::glob(pattern, UI_ACTION))
        });
        if decision.verdict == Verdict::Deny {
            *gate = audit::Gate {
                decision: audit::DENIED,
                decided_by,
                rule: decision.rule_id.clone(),
                reason: decision.reason.clone(),
                approval_id: None,
            };
            return Err(OperatorCallError::Denied(refusal(decision)));
        }
        if denied_by_entry {
            let reason =
                format!("the operator does not allow the UI of plugin {plugin} ([[plugin]] deny)");
            *gate = audit::Gate {
                decision: audit::DENIED,
                decided_by: "operator",
                rule: Some(format!("plugin.{plugin}.deny")),
                reason: Some(reason.clone()),
                approval_id: None,
            };
            return Err(OperatorCallError::Denied(reason));
        }
        *gate = audit::Gate {
            decision: audit::POLICY_ALLOWED,
            decided_by,
            rule: decision.rule_id.clone(),
            reason: decision.reason.clone(),
            approval_id: None,
        };
        let supervisor = access.manager.plugins();
        let lease = match supervisor.lease(plugin, &ctx.channel).await {
            Ok(lease) => lease,
            Err(e) => {
                return Err(OperatorCallError::Unreachable(format!(
                    "plugin {plugin} cannot be reached: {e}"
                )))
            }
        };
        let endpoint = &lease.endpoint;
        // Lent as a tool call is, by the query parameters the build's
        // `select`s name, and never what is lent only on approval.
        let query = request.path.split_once('?').map(|(_, q)| q);
        let credentials = match crate::plugin::lending::page_arguments(&endpoint.manifest, query)
            .map_err(|why| {
                NotLent::Refused(format!(
                    "nothing ran: plugin {plugin} cannot be lent what this request names on \
                     channel {}: {why}",
                    ctx.channel
                ))
            })
            .and_then(|selects| self.lent(ctx, &endpoint.manifest, &selects, false))
        {
            Ok(credentials) => credentials,
            Err(NotLent::Refused(why)) => {
                gate.reason = Some(why.clone());
                return refuse(gate, OperatorCallError::Request(why));
            }
            Err(NotLent::Broker(why)) => return Err(OperatorCallError::Unreachable(why)),
        };
        let caller = context::CallerId {
            kind: "operator",
            session: None,
            harness: None,
            plugin: None,
        };
        let draft = context::Draft {
            plugin,
            token: &endpoint.token,
            tool: UI_TOOL,
            arguments: args,
            caller: &caller,
            channel: &ctx.channel,
            approval_id: None,
            credentials: &credentials,
            preview_token: None,
        };
        let signed = context::build(&draft, crate::store::now_ms(), |b| supervisor.sign(b))
            .map_err(OperatorCallError::Unreachable)?;
        let header = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            signed.to_string(),
        );
        let timeout = std::time::Duration::from_secs(
            entry
                .map(crate::config::PluginEntry::timeout_secs)
                .unwrap_or(crate::config::PluginEntry::DEFAULT_TIMEOUT_SECS),
        );
        let http = upstream::unix_client(&endpoint.socket);
        let url = format!(
            "{}{}",
            upstream::PLUGIN_URL.trim_end_matches('/'),
            request.path
        );
        let mut outgoing = http
            .request(request.method, url)
            .timeout(timeout)
            .header(UI_CONTEXT_HEADER, header);
        for (key, value) in &request.headers {
            outgoing = outgoing.header(key.as_str(), value.as_str());
        }
        let response =
            outgoing.body(request.body).send().await.map_err(|e| {
                OperatorCallError::Tool(format!("plugin {plugin} did not answer: {e}"))
            })?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter(|(k, _)| UI_ANSWER_HEADERS.contains(&k.as_str()))
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        if response
            .content_length()
            .is_some_and(|n| n > UI_ANSWER_MAX as u64)
        {
            return Err(OperatorCallError::Tool(format!(
                "plugin {plugin} answered more than {UI_ANSWER_MAX} bytes"
            )));
        }
        let mut body = Vec::new();
        let mut stream = response;
        while let Some(chunk) = stream.chunk().await.map_err(|e| {
            OperatorCallError::Tool(format!("plugin {plugin}'s answer broke off: {e}"))
        })? {
            if body.len() + chunk.len() > UI_ANSWER_MAX {
                return Err(OperatorCallError::Tool(format!(
                    "plugin {plugin} answered more than {UI_ANSWER_MAX} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        drop(lease);
        Ok(UiAnswer {
            status,
            headers,
            body: body.into(),
        })
    }

    /// The plugin's preview of a waiting call as the operator has edited it
    /// (`edited`, the whole call), asked as [`Self::approval_preview`] asks
    /// when the card opens: its token replaces the one kept, so an answer
    /// naming this preview applies against what it showed. An edit an answer
    /// would refuse is refused here the same way, before the plugin is asked.
    /// For a plugin's tool it carries the card's `plugin` context for the
    /// edit too, so "Lent on allow" follows the edit, previews or not.
    pub async fn preview_edit(
        &self,
        approval: &crate::store::approvals::ApprovalRow,
        edited: &Value,
    ) -> Result<Option<Value>, Vec<schema::FieldError>> {
        let original = serde_json::from_str::<Value>(&approval.arguments).unwrap_or(Value::Null);
        let errors = schema::check_edit(&approval.tool, &original, edited);
        if !errors.is_empty() {
            return Err(errors);
        }
        let preview = self.approval_preview(approval, edited).await;
        // What Allow would lend the edit, from the dispatch's own function.
        let Some(plugin) = self.plugin_approval_context(approval, Some(edited)) else {
            return Ok(preview);
        };
        let mut out = preview.unwrap_or_else(|| json!({}));
        if let Some(map) = out.as_object_mut() {
            map.insert("plugin".into(), plugin);
        }
        Ok(Some(out))
    }

    /// The input schema a plugin's running build gave one of its tools, with
    /// the `operation_id` the node adds when it is consequential.
    pub fn plugin_schema(&self, name: &str) -> Option<Value> {
        let pt = self.plugin_tool(name)?;
        let listed = self.session.get()?.manager.plugins().listed(&pt.plugin)?;
        (listed.digest == pt.digest)
            .then(|| listed.tools.iter().find(|t| t.remote == pt.tool))
            .flatten()
            .map(|t| match pt.consequential() {
                true => with_operation_id(&t.input_schema),
                false => t.input_schema.clone(),
            })
    }

    /// How a card for one of a plugin's tools is laid out, from the schema
    /// its running build listed for it ([`schema::plugin_presentation`]).
    pub fn plugin_presentation(&self, name: &str) -> Option<schema::Presentation> {
        schema::plugin_presentation(name, &self.plugin_schema(name)?)
    }

    /// The layout of a held call's card: the node's own for its tools, the
    /// plugin's marked arguments for a plugin's.
    pub fn presentation_for(&self, tool: &str, args: &Value) -> schema::Presentation {
        self.plugin_presentation(tool)
            .unwrap_or_else(|| schema::presentation_for(tool, args))
    }

    /// `text` converted by the plugin that owns `format`, on `channel`: to
    /// Markdown from `format`, or (`to` = `format`) back from Markdown. A pure
    /// function of the text: no caller context, nothing lent.
    pub async fn convert(
        &self,
        channel: &str,
        format: &str,
        to: &str,
        text: &str,
    ) -> Result<String, String> {
        let access = self
            .session
            .get()
            .ok_or("plugins are not available on this node")?;
        if text.len() > CONVERT_MAX {
            return Err(format!("the text is larger than {CONVERT_MAX} bytes"));
        }
        let owner = access
            .store
            .plugins_list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|row| row.state != crate::store::plugins::REMOVED)
            .find(|row| {
                row.active
                    .as_ref()
                    .is_some_and(|b| b.manifest.formats.iter().any(|f| f == format))
            })
            .map(|row| row.name)
            .ok_or_else(|| format!("no installed plugin owns the format {format}"))?;
        let lease = access
            .manager
            .plugins()
            .lease(&owner, channel)
            .await
            .map_err(|e| format!("plugin {owner} cannot be reached: {e}"))?;
        let params = json!({ "format": format, "to": to, "text": text });
        let result = self
            .upstream
            .plugin_request(
                &lease.endpoint,
                &owner,
                channel,
                CONVERT_METHOD,
                || Ok(params.clone()),
                CONVERT_TIMEOUT,
            )
            .await
            .map_err(|f| f.describe(&format!("plugin {owner}")))?;
        let converted = result["text"]
            .as_str()
            .ok_or_else(|| format!("plugin {owner} answered a conversion with no text"))?;
        if converted.len() > CONVERT_MAX {
            return Err(format!(
                "plugin {owner} answered a conversion larger than {CONVERT_MAX} bytes"
            ));
        }
        Ok(converted.to_string())
    }

    /// A held call's prose, rendered as Markdown by the plugin that owns the
    /// format it is written in: `rendered` by field, and `rendered_items`,
    /// by field, for each item of an item list. A failure is `render_error`,
    /// and the page shows the source, which stays in the arguments either way.
    pub async fn approval_rendering(
        &self,
        channel: &str,
        shown: &schema::Presentation,
        args: &Value,
    ) -> Option<Value> {
        let format = shown.plugin_format.as_deref()?;
        let render = |fields: &Value| {
            let fields = fields.clone();
            async move {
                let mut out = serde_json::Map::new();
                for field in &shown.prose_fields {
                    if let Some(text) = fields.get(field.as_ref()).and_then(Value::as_str) {
                        let md = self.convert(channel, format, "markdown", text).await?;
                        out.insert(field.to_string(), Value::from(md));
                    }
                }
                Ok::<_, String>(Value::Object(out))
            }
        };
        let outcome = async {
            let rendered = render(args).await?;
            let items = match shown.item_list.as_deref().and_then(|l| args.get(l)) {
                Some(Value::Array(items)) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        out.push(render(item).await?);
                    }
                    Some(out)
                }
                _ => None,
            };
            Ok::<_, String>((rendered, items))
        }
        .await;
        Some(match outcome {
            Ok((rendered, items)) => json!({ "rendered": rendered, "rendered_items": items }),
            Err(error) => {
                tracing::info!(%format, %error, "a card's prose was not rendered");
                json!({ "render_error": error })
            }
        })
    }

    /// What a card for a plugin's tool shows besides the call: the build it
    /// was asked on, what the plugin says the tool does and reaches, which it
    /// never decides anything by, and what the call is lent once approved.
    /// `replaced_by` names the build active now when that is another, which
    /// an approved run is refused on. What is lent is computed by the
    /// dispatch's own function, from `edited` when the operator is drafting
    /// an edit, else from the call as asked or as it ran.
    pub fn plugin_approval_context(
        &self,
        approval: &crate::store::approvals::ApprovalRow,
        edited: Option<&Value>,
    ) -> Option<Value> {
        let pt = self.plugin_tool(&approval.tool)?;
        let access = self.session.get()?;
        let asked_on = access
            .store
            .approval_plugin_digest(&approval.id)
            .ok()
            .flatten()
            .filter(|d| *d != pt.digest);
        // The build asked on, when the node still keeps it for rollback.
        let kept = asked_on.as_ref().and_then(|d| {
            access
                .store
                .plugin_get(&pt.plugin)
                .ok()
                .flatten()
                .and_then(|row| row.previous)
                .filter(|b| b.digest == *d)
        });
        let (version, manifest) = match (&asked_on, &kept) {
            (None, _) => (pt.manifest.version.clone(), &pt.manifest),
            (Some(_), Some(build)) => (build.manifest.version.clone(), &build.manifest),
            // Gone from the node: what it declared is not known any more.
            (Some(digest), None) => (format!("build {}", short_digest(digest)), &pt.manifest),
        };
        let replaced_by = asked_on.as_ref().map(|_| pt.manifest.version.clone());
        let tool = manifest
            .tools
            .iter()
            .find(|t| t.name == pt.tool)
            .or_else(|| pt.manifest.tools.iter().find(|t| t.name == pt.tool))?;
        // What Allow lends, from the call as it would run: the operator's
        // edit while it is drafted (`edited`), what ran once decided.
        let asked = serde_json::from_str::<Value>(&approval.arguments).unwrap_or(Value::Null);
        let ran = approval
            .edited_arguments
            .as_deref()
            .and_then(|e| serde_json::from_str::<Value>(e).ok());
        let args = edited.or(ran.as_ref()).unwrap_or(&asked);
        let broker = self.broker.read().unwrap();
        let available = broker.available_to(&approval.channel, &approval.node_id);
        let credentials: Vec<Value> =
            crate::plugin::lending::lending(manifest, &available, args, true)
                .into_iter()
                .map(|l| {
                    json!({
                        "name": l.credential.name,
                        "lend": l.credential.lend.as_str(),
                        "select": l.credential.select,
                        "bindings": l.bindings,
                        "refusal": l.refusal,
                    })
                })
                .collect();
        // What a grant allowing exactly this call names, for a consequential
        // tool: it is asked unless one does.
        let grant_target = tool
            .consequential
            .then(|| serde_json::from_str::<Value>(&approval.arguments).ok())
            .flatten()
            .and_then(|args| plugin_consequential(&approval.tool, &args))
            .map(|(_, target, _)| target);
        Some(json!({
            "name": pt.plugin,
            "version": version,
            "replaced_by": replaced_by,
            "tool": pt.tool,
            "hint": tool.hint.as_str(),
            "consequential": tool.consequential,
            "grant_target": grant_target,
            "egress": manifest.egress,
            "credentials": credentials,
        }))
    }

    /// The `[[mcp_server]]` a namespaced tool belongs to, as the node is
    /// configured now, and the server's own name for the tool.
    fn upstream_tool(&self, name: &str) -> Option<(crate::config::McpServer, String)> {
        let live = self.cfg.live();
        let (server, rest) = upstream::server_of(&live.mcp_server, name)?;
        let remote = self
            .upstream
            .remote_name(server, name)
            .unwrap_or_else(|| rest.to_string());
        Some((server.clone(), remote))
    }

    /// How a call to another server's tool is answered under `policy`, from
    /// the policy's own standing for it; `None` for a tool that is not
    /// another server's.
    pub fn upstream_decision(
        &self,
        policy: &Policy,
        name: &str,
        standing: crate::policy::Standing,
    ) -> Option<Decision> {
        let policy_decision = Decision {
            verdict: standing.verdict,
            rule_id: standing.rule_id,
            reason: standing.reason,
        };
        if let Some(pt) = self.plugin_tool(name) {
            let entry = upstream::Entry::plugin(&pt.plugin, pt.entry.as_ref());
            let decision = upstream::decide(&entry, &pt.tool, policy, policy_decision);
            // As [`Self::decide`] has it: allowed by name is not allowed.
            if pt.consequential() && decision.verdict == Verdict::Allow {
                return Some(Decision {
                    verdict: Verdict::Ask,
                    rule_id: None,
                    reason: Some(format!(
                        "{name} is consequential: each call is asked unless a {} grant names \
                         that one call",
                        crate::authority::PLUGIN_CALL
                    )),
                });
            }
            return Some(decision);
        }
        let (server, remote) = self.upstream_tool(name)?;
        Some(upstream::decide(
            &upstream::Entry::server(&server),
            &remote,
            policy,
            policy_decision,
        ))
    }

    /// The input schema another server gave one of its tools, as offered.
    pub fn upstream_schema(&self, name: &str) -> Option<Value> {
        let live = self.cfg.live();
        let (server, _) = upstream::server_of(&live.mcp_server, name)?;
        self.upstream.input_schema(server, name)
    }

    /// How a call to each tool a plugin's build declares is answered now,
    /// for the operator, as [`Self::upstream_status`] says it of an
    /// `[[mcp_server]]`'s: `setting` is what the operator's `[[plugin]]`
    /// entry says of it, and `decision` and `rule_id` are the answer with
    /// the signed policy. Decided on no particular channel, so a rule scoped
    /// to a channel can answer otherwise there.
    pub fn plugin_tool_status(&self, manifest: &crate::plugin::manifest::Manifest) -> Vec<Value> {
        let live = self.cfg.live();
        let entry = live.plugin.iter().find(|p| p.name == manifest.name);
        let entry = upstream::Entry::plugin(&manifest.name, entry);
        let policy = self.policy.read();
        manifest
            .tools
            .iter()
            .map(|tool| {
                let name = format!("{}{}{}", manifest.name, upstream::SEPARATOR, tool.name);
                let policy_decision = policy.decide(&Request {
                    channel: "",
                    action: &name,
                    kind: Some(TOOL_KIND),
                    resource: None,
                    command: None,
                    arguments: None,
                    doc_kinds: None,
                });
                let decision = upstream::decide(&entry, &tool.name, &policy, policy_decision);
                json!({
                    "name": name,
                    "tool": tool.name,
                    "hint": tool.hint.as_str(),
                    "consequential": tool.consequential,
                    "setting": entry.setting(&tool.name).map(upstream::verdict_name),
                    "decision": upstream::verdict_name(decision.verdict),
                    "rule_id": decision.rule_id,
                })
            })
            .collect()
    }

    /// Every `[[mcp_server]]` as the node sees it now, for the operator.
    pub fn upstream_status(&self) -> Vec<Value> {
        self.upstream
            .status(&self.cfg.live().mcp_server, &self.policy.read())
    }

    /// List again the servers whose listing is stale, so a `tools/list` and
    /// a session's orientation see what they offer now.
    pub async fn refresh_upstream(&self, node_id: &str, force: bool) {
        let servers = self.cfg.live().mcp_server.clone();
        if servers.is_empty() {
            return;
        }
        self.upstream
            .refresh(&servers, &self.broker, node_id, force)
            .await;
    }

    /// Whether a call to another server's tool can be made for this caller
    /// at all: the server serves its channel, the channel may use its
    /// credential, and the server has listed the tool. Checked before the
    /// call is decided, so an unreachable server or a tool it does not have
    /// never becomes an approval.
    async fn upstream_ready(&self, ctx: &CallContext, name: &str) -> Result<(), String> {
        let Some((server, _)) = self.upstream_tool(name) else {
            return Ok(());
        };
        if !server.serves(&ctx.channel) {
            return Err(format!(
                "MCP server {} is not offered on channel {}",
                server.name, ctx.channel
            ));
        }
        if let Some(credential) = &server.credential {
            let broker = self.broker.read().unwrap();
            if !broker
                .available_to(&ctx.channel, &ctx.node_id)
                .contains(&credential.as_str())
            {
                return Err(format!(
                    "MCP server {} needs credential {credential}, which channel {} may not use",
                    server.name, ctx.channel
                ));
            }
        }
        if self.upstream.remote_name(&server, name).is_none() {
            self.refresh_upstream(&ctx.node_id, false).await;
        }
        if self.upstream.remote_name(&server, name).is_some() {
            return Ok(());
        }
        Err(match self.upstream.unavailable(&server) {
            Some(why) => format!(
                "MCP server {} cannot be reached for {name}: {why}",
                server.name
            ),
            None => format!("MCP server {} lists no tool {name}", server.name),
        })
    }

    pub async fn call(&self, ctx: &CallContext, name: &str, args: &Value) -> Result<Value, String> {
        let mut gate = audit::Gate::default();
        // This node's own tools take a record's name where they take its id,
        // and say each record's name beside its id. Another server's tools
        // are passed through as they are.
        let own = self.upstream_tool(name).is_none() && self.plugin_tool(name).is_none();
        let store = self.session.get().map(|access| access.store.clone());
        let named = match (&store, own) {
            (Some(store), true) => names::resolve_args(store, &ctx.channel, name, args)?,
            _ => None,
        };
        let args = named.as_ref().unwrap_or(args);
        let result = self.call_gated(ctx, name, args, &mut gate).await;
        // A session's calls are its audit; a session-less caller's channel
        // log already records each call it makes.
        if let (Some(session_id), Some(access)) = (ctx.session_id(), self.session.get()) {
            if gate.decision == audit::REFUSED && gate.reason.is_none() {
                gate.reason = result.as_ref().err().cloned();
            }
            access.manager.record_event(
                session_id,
                crate::session::state::event_kind::TOOL_DECIDED,
                audit::record(name, args, &gate, &result),
            );
        } else if gate.rule.as_deref() == Some("own-change") {
            // That log says what was called, not who let it run. A forge
            // write the node let an external caller make without asking
            // anyone has no approval to say so, so its decision is logged
            // beside the call.
            if let Some(access) = self.session.get() {
                access.manager.record_for(
                    ctx,
                    crate::session::state::event_kind::TOOL_DECIDED,
                    None,
                    audit::record(name, args, &gate, &result),
                );
            }
        }
        match (store, own) {
            (Some(store), true) => result.map(|mut v| {
                names::annotate(&store, &mut v);
                v
            }),
            _ => result,
        }
    }

    /// [`Self::call`], saying in `gate` how it was decided.
    async fn call_gated(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
        gate: &mut audit::Gate,
    ) -> Result<Value, String> {
        let defaulted = self.with_defaults(ctx, name, args)?;
        let args = defaulted.as_ref().unwrap_or(args);
        // A plan session's own plan document is the phase's artifact: writing
        // or editing that one slug is what the session exists to do, so it is
        // not asked.
        // A delete, and a write that converts a document or drops files of
        // its bundle, are always the operator's, whatever the policy says;
        // so is an amendment, which rewrites a memory in place with no
        // nightly batch behind it.
        let always_asked = name == docs::DOC_DELETE
            || name == memory::AMEND
            || name == docs::DOC_WRITE
                && self
                    .session
                    .get()
                    .and_then(|access| docs::write_consequence(&access.store, &ctx.channel, args))
                    .is_some();
        let plan_write = matches!(name, docs::DOC_WRITE | docs::DOC_EDIT)
            && !always_asked
            && self.is_plan_artifact(ctx, args);
        let audience = self.audience(ctx);
        if !audience.offers(name) {
            return Err(audience.refusal(name));
        }
        // A directive, another channel's memory, or one no longer live is
        // refused before it is held, as a correction's target is.
        if name == memory::AMEND {
            if let Some(access) = self.session.get() {
                memory::amend_target(&access.store, &ctx.channel, args)?;
            }
        }
        if matches!(
            name,
            operator::ASK
                | operator::NOTIFY
                | operator::REPORT
                | operator::REPORT_STATUS
                | operator::QUESTION_STATUS
        ) {
            let access = self
                .session
                .get()
                .ok_or("operator interventions are not available on this node")?;
            *gate = audit::Gate::node("operator-intervention");
            return operator::call(&access.store, &access.manager, &self.cfg, ctx, name, args)
                .await;
        }
        if name == egress::REQUEST {
            let access = self
                .session
                .get()
                .ok_or("egress requests are not available on this node")?;
            access
                .manager
                .caller_active(ctx)
                .map_err(|error| error.to_string())?;
            let result = egress::call(access, ctx, args).await;
            *gate = match audit::approval_of(&result) {
                Some(_) => audit::Gate::asked(&result, "node", Some("egress".into())),
                None => audit::Gate::node("egress"),
            };
            return result;
        }
        if matches!(name, setup::DRAFT | setup::TRY | setup::PROPOSE) {
            let access = self
                .session
                .get()
                .ok_or("repository setup is not available on this node")?;
            access
                .manager
                .caller_active(ctx)
                .map_err(|error| error.to_string())?;
            if name == setup::PROPOSE {
                // Always the operator's, whatever the policy says: an agent
                // does not write its own repository's environment.
                let asked = setup::proposal(access, ctx, args)?;
                let result = self.request_approval(ctx, name, &asked).await;
                *gate = audit::Gate::asked(&result, "node", Some("always-asked".into()));
                return result;
            }
            if name == setup::TRY {
                // Building a repository's Dockerfile runs it with the network,
                // outside the boundary: the operator's, as when they name one
                // in an entry themselves.
                if let Some(asked) = setup::build_to_ask(access, ctx, args).await? {
                    let result = self.request_approval(ctx, name, &asked).await;
                    *gate = audit::Gate::asked(&result, "node", Some("always-asked".into()));
                    return result;
                }
            }
            *gate = audit::Gate::node("repository-setup");
            return setup::call(access, ctx, name, args).await;
        }
        if name == show::SHOW {
            let access = self
                .session
                .get()
                .ok_or("showing work is not available on this node")?;
            access
                .manager
                .caller_active(ctx)
                .map_err(|error| error.to_string())?;
            *gate = audit::Gate::node("show-work");
            return show::call(access, ctx, args).await;
        }
        if name == branch::FETCH {
            let access = self
                .session
                .get()
                .ok_or("fetching a branch is not available on this node")?;
            access
                .manager
                .caller_active(ctx)
                .map_err(|error| error.to_string())?;
            *gate = audit::Gate::node("fetch-branch");
            return branch::call(access, ctx, args).await;
        }
        if name == approvals::STATUS {
            let access = self
                .session
                .get()
                .ok_or("approvals are not available on this node")?;
            *gate = audit::Gate::node("approval-status");
            return approvals::status(access, ctx, args).await;
        }
        if self.is_consequential(name) && self.consequential_scope(name, args).is_none() {
            return Err(consequential_refusal(name, args));
        }
        self.upstream_ready(ctx, name).await?;
        self.plugin_ready(ctx, name, args).await?;
        // A workflow the node does not start is refused before the operator
        // is asked about it, not after.
        if let Some(refusal) = self.refusal_before_asking(ctx, name, args) {
            return Err(refusal);
        }
        // Drafting the change of a review this session submitted or is
        // reviewing, or an external caller's harness submitted, and pushed to
        // or opened, is not asked: drafting blocks
        // nothing that cannot be undone, and the agent is the first to know
        // another revision is coming. Marking it ready is asked like any
        // other call: a ready change can be merged at a revision nobody
        // approved. A review session may touch no other change.
        let own_change = matches!(name, github::PR_READY | gitlab::MR_READY)
            .then(|| self.own_change(ctx, name, args))
            .flatten();
        if own_change.is_none()
            && matches!(name, github::PR_READY | gitlab::MR_READY)
            && audience == Audience::Session(crate::session::Phase::Review)
        {
            return Err(format!(
                "a review session marks only the change of the review it is reviewing; {name} \
                 names another"
            ));
        }
        let drafts_own_change = own_change.is_some() && github::ready_arg(args) == Ok(false);
        if plan_write {
            *gate = audit::Gate::node("plan-artifact");
        } else {
            let decision = self.decide(ctx, name, args);
            match decision.verdict {
                Verdict::Allow if !always_asked => {
                    *gate = audit::Gate {
                        decision: audit::POLICY_ALLOWED,
                        decided_by: self.decided_by(&decision),
                        rule: decision.rule_id.clone(),
                        reason: decision.reason.clone(),
                        approval_id: None,
                    };
                }
                Verdict::Deny => {
                    *gate = audit::Gate {
                        decision: audit::DENIED,
                        decided_by: self.decided_by(&decision),
                        rule: decision.rule_id.clone(),
                        reason: decision.reason.clone(),
                        approval_id: None,
                    };
                    return Err(refusal(decision));
                }
                Verdict::Ask if drafts_own_change => {
                    *gate = audit::Gate::node("own-change");
                }
                // Nothing waits on the operator: the call is held as an
                // approval and the caller is told so at once.
                Verdict::Allow | Verdict::Ask => {
                    if let Some(refusal) = self.missing_required(ctx, name, args) {
                        return Err(refusal);
                    }
                    // A merge at a head the pull request no longer has could
                    // only fail once allowed, so it is never asked.
                    if name == github::PR_MERGE {
                        if let Some(refusal) =
                            github::merge_head_refusal(&self.broker, &self.http, ctx, args).await
                        {
                            return Err(refusal);
                        }
                    }
                    let pinned = match name {
                        docs::DOC_WRITE | docs::DOC_DELETE => self
                            .session
                            .get()
                            .and_then(|access| docs::pinned(&access.store, &ctx.channel, args)),
                        _ => None,
                    };
                    let result = self
                        .request_approval(ctx, name, pinned.as_ref().unwrap_or(args))
                        .await;
                    let (by, rule) = if always_asked {
                        ("node", Some("always-asked".to_string()))
                    } else {
                        (self.decided_by(&decision), decision.rule_id.clone())
                    };
                    *gate = audit::Gate {
                        reason: decision.reason.clone(),
                        ..audit::Gate::asked(&result, by, rule)
                    };
                    return result;
                }
            }
        }
        if let Some(access) = self.session.get() {
            if let Err(error) = access.manager.caller_active(ctx) {
                *gate = audit::Gate::default();
                return Err(error.to_string());
            }
        }
        let result = self.dispatch(ctx, name, args, None).await;
        // Kept even when the change was already that way: the reason is
        // what the review page says it waits on.
        if let (Ok(_), Some((review, by)), Some(access)) =
            (&result, &own_change, self.session.get())
        {
            review::record_change_ready(
                &access.manager,
                review,
                github::ready_arg(args).unwrap_or(true),
                by,
                args.get("reason").and_then(Value::as_str),
            );
        }
        result
    }

    /// The review whose change a `pr_ready` or `mr_ready` call names, when
    /// the caller owns it: the execute session that submitted it, the
    /// review session reviewing it, or an external caller on the channel of
    /// a review [`review::caller_owns`] gives it (one an external harness
    /// submitted). With who that is, as the review page says it (`agent` or
    /// `reviewer`).
    ///
    /// Naming the change is not enough: `submit_review` adopts any open
    /// change of the repository it is pointed at, so a review owns one only
    /// once a publication of it pushed to it or opened it, which the
    /// operator approved.
    fn own_change(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
    ) -> Option<(crate::store::ReviewRow, &'static str)> {
        let access = self.session.get()?;
        let (provider, project, number) = match name {
            github::PR_READY => ("github", args["repo"].as_str()?, args["number"].as_u64()?),
            _ => ("gitlab", args["project"].as_str()?, args["iid"].as_u64()?),
        };
        let names = |r: &crate::store::ReviewRow| {
            serde_json::from_str::<crate::review::publish::Target>(&r.target).is_ok_and(|t| {
                t.provider == provider
                    && t.project.eq_ignore_ascii_case(project)
                    && t.change.is_some_and(|change| change.number == number)
            }) && access
                .store
                .publications_for_review(&r.id)
                .is_ok_and(|rows| {
                    rows.iter()
                        .any(|row| row.pushed_sha.is_some() || row.state == "opened")
                })
        };
        let Some(session_id) = ctx.session_id() else {
            // An external caller has no session: it owns what the channel's
            // external harnesses submitted, the most recent first.
            if !ctx.is_external() {
                return None;
            }
            return access
                .store
                .reviews_naming_change(&ctx.channel, provider, project, number)
                .ok()?
                .into_iter()
                .rev()
                .find(|r| {
                    review::caller_owns(&access.store, ctx, &r.channel, r.session_id.as_deref())
                        && names(r)
                })
                .map(|review| (review, "agent"));
        };
        let session = access.store.get_session(session_id).ok()??;
        if session.phase == crate::session::Phase::Review.as_str() {
            let review = access
                .store
                .get_review(session.review_id.as_deref()?)
                .ok()??;
            return names(&review).then_some((review, "reviewer"));
        }
        access
            .store
            .reviews_of_session(&session.id)
            .ok()?
            .into_iter()
            .rev()
            .find(|r| names(r))
            .map(|review| (review, "agent"))
    }

    /// Whether a decision was the signed policy's or an operator's scoped
    /// grant, which an authority decision names by its id.
    fn decided_by(&self, decision: &Decision) -> &'static str {
        let grant = decision.rule_id.as_deref().is_some_and(|id| {
            self.session
                .get()
                .and_then(|access| access.store.authority_grant(id).ok().flatten())
                .is_some()
        });
        if grant {
            "grant"
        } else {
            "policy"
        }
    }

    /// Run a call that is allowed: by policy, by a grant, or by the operator
    /// (`approval`, the card they allowed). Everything a call is checked
    /// against is checked again here, because an approved call runs long
    /// after it was asked.
    async fn dispatch(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
        approval: Option<&str>,
    ) -> Result<Value, String> {
        // The operator may have edited the arguments in the prompt; a default
        // they removed is put back the same visible way.
        let defaulted = self.with_defaults(ctx, name, args)?;
        let args = defaulted.as_ref().unwrap_or(args);
        let gated = GatedCall {
            one_shot: approval.is_some(),
        };
        if self.is_consequential(name) && self.consequential_scope(name, args).is_none() {
            return Err(consequential_refusal(name, args));
        }
        if let Some(refusal) = self.refusal_before_asking(ctx, name, args) {
            return Err(refusal);
        }
        self.revalidate_consequential(ctx, name, args, gated.one_shot)?;
        let action_record = self.begin_consequential(ctx, name, args, gated.one_shot)?;
        if let Some(ActionRecord::Replay(outcome)) = &action_record {
            return Ok(outcome.clone());
        }
        let before_mutation = || {
            let Some(ActionRecord::New(id)) = &action_record else {
                return Ok(());
            };
            let (action, target, revision) = self
                .consequential_scope(name, args)
                .expect("consequential action record has canonical arguments");
            let policy = self.policy.read();
            let live = self.cfg.live();
            let tool = policy.decide(&Request {
                channel: &ctx.channel,
                action: name,
                kind: Some(TOOL_KIND),
                resource: None,
                command: None,
                arguments: Some(&self.policy_arguments(name, args)),
                doc_kinds: Some(&live.docs.kind),
            });
            if tool.verdict == Verdict::Deny {
                return Err(refusal(tool));
            }
            let access = self.session.get().ok_or("authority requires a session")?;
            let decision = crate::authority::decide(
                access.store.as_ref(),
                &policy,
                &crate::authority::AuthorityQuery {
                    channel: &ctx.channel,
                    session_id: ctx.session_id(),
                    action,
                    target: &target,
                    revision: revision.as_deref(),
                    args,
                },
            )?;
            match decision.verdict {
                Verdict::Allow => {}
                Verdict::Ask if gated.one_shot => {}
                Verdict::Deny => return Err(refusal(decision)),
                Verdict::Ask => {
                    return Err(format!(
                        "{action} for {target} needs a current scoped authority grant"
                    ))
                }
            }
            access
                .store
                .authority_action_set_grant(id, decision.rule_id.as_deref())
                .map_err(|e| e.to_string())
        };
        let result = match name {
            gitlab::MR_STATUS
            | gitlab::MR_COMMENT
            | gitlab::MR_MERGE
            | gitlab::MR_READY
            | gitlab::PIPELINE_STATUS
            | gitlab::PIPELINE_WAIT
            | gitlab::PIPELINE_LIST_BY_SHA
            | gitlab::JOB_TRACE
            | gitlab::JOB_PLAY
            | gitlab::PIPELINE_RUN
            | gitlab::DEPLOY
            | gitlab::MR_DISCUSSIONS
            | gitlab::MR_REPLY
            | gitlab::MR_FOR_BRANCH
            | gitlab::PIPELINE_RETRY => {
                gitlab::call(
                    &self.broker,
                    &self.http,
                    ctx,
                    name,
                    args,
                    Some(&before_mutation),
                )
                .await
            }
            review::SUBMIT
            | review::STATUS
            | review::SUBMIT_REPORT
            | review::REPORT_STATUS
            | review::VERDICT => {
                let access = self
                    .session
                    .get()
                    .ok_or("review tools are not available on this node")?;
                review::call(&access.store, &access.manager, ctx, name, args).await
            }
            memory::RECALL | memory::RETAIN => {
                let access = self
                    .session
                    .get()
                    .ok_or("memory is not available on this node")?;
                memory::call(self, access, ctx, name, args).await
            }
            memory::AMEND => {
                if !gated.one_shot {
                    return Err(format!("{name} runs only once the operator allows it"));
                }
                let access = self
                    .session
                    .get()
                    .ok_or("memory is not available on this node")?;
                memory::amend(access, ctx, args)
            }
            setup::PROPOSE => {
                if !gated.one_shot {
                    return Err(format!(
                        "{name} is written only once the operator allows it"
                    ));
                }
                let access = self
                    .session
                    .get()
                    .ok_or("repository setup is not available on this node")?;
                setup::write(access, ctx, args)
            }
            setup::TRY => {
                if !gated.one_shot {
                    return Err(format!(
                        "{name} builds an image only once the operator allows it"
                    ));
                }
                let access = self
                    .session
                    .get()
                    .ok_or("repository setup is not available on this node")?;
                setup::start_allowed(access, ctx, args)
            }
            work::WORK_READY
            | work::WORK_DISCOVER
            | work::WORK_CLOSE
            | work::WORK_LIST
            | work::WORK_SHOW
            | work::WORK_UPDATE
            | work::WORK_DELETE => {
                let access = self
                    .session
                    .get()
                    .ok_or_else(|| "node not ready".to_string())?;
                work::call(&self.cfg, access, ctx, name, args).await
            }
            docs::DOC_READ
            | docs::DOC_SEARCH
            | docs::DOC_WRITE
            | docs::DOC_EDIT
            | docs::DOC_DELETE => {
                let access = self
                    .session
                    .get()
                    .ok_or("documents are not available on this node")?;
                docs::call(self, access, ctx, name, args).await
            }
            github::PR_STATUS
            | github::PR_COMMENT
            | github::ISSUE_COMMENT
            | github::RUN_STATUS
            | github::PR_MERGE
            | github::PR_READY
            | github::PR_THREADS
            | github::PR_REPLY
            | github::PR_FOR_BRANCH
            | github::RUN_LOGS
            | github::RUN_RERUN
            | github::RUN_WAIT
            | github::RUN_DISPATCH => {
                github::call(
                    &self.broker,
                    &self.http,
                    ctx,
                    name,
                    args,
                    Some(&before_mutation),
                )
                .await
            }
            services::START | services::STATUS => {
                let access = self
                    .session
                    .get()
                    .ok_or("services are not available on this node")?;
                services::call(access, ctx, name, args).await
            }
            other => match (self.plugin_tool(other), self.upstream_tool(other)) {
                (Some(pt), _) => {
                    // Decided again at the last moment, as another server's
                    // tool is.
                    let decision = self.decide(ctx, other, args);
                    match decision.verdict {
                        Verdict::Deny => Err(refusal(decision)),
                        Verdict::Ask if !gated.one_shot => {
                            Err(format!("{other} runs only once the operator allows it"))
                        }
                        Verdict::Allow | Verdict::Ask => {
                            // A consequential call records the grant that
                            // let it run, read again just before it is sent;
                            // a refusal there finishes its record as failed.
                            match before_mutation() {
                                Ok(()) => self.call_plugin(ctx, &pt, args, approval).await,
                                Err(refused) => Err(refused),
                            }
                        }
                    }
                }
                (None, Some((server, remote))) => {
                    // Decided again at the last moment: an entry edited to
                    // deny while the call waited on the operator holds.
                    let decision = self.decide(ctx, other, args);
                    match decision.verdict {
                        Verdict::Deny => Err(refusal(decision)),
                        Verdict::Ask if !gated.one_shot => {
                            Err(format!("{other} runs only once the operator allows it"))
                        }
                        Verdict::Allow | Verdict::Ask => {
                            self.upstream
                                .call(
                                    &server,
                                    &self.broker,
                                    &ctx.channel,
                                    &ctx.node_id,
                                    &remote,
                                    args,
                                )
                                .await
                        }
                    }
                }
                (None, None) => Err(format!("no tool named {other}")),
            },
        };
        let result = match (name, result) {
            (
                gitlab::PIPELINE_RUN
                | gitlab::JOB_PLAY
                | gitlab::DEPLOY
                | gitlab::PIPELINE_RETRY
                | github::RUN_RERUN
                | github::RUN_DISPATCH,
                Ok(started),
            ) => Ok(crate::follow::subscribe(self, ctx, name, args, started).await),
            (_, result) => result,
        };
        if let Some(ActionRecord::New(id)) = action_record {
            let (state, outcome) = match &result {
                Ok(value) => ("succeeded", value.to_string()),
                Err(error) if remote_outcome_unknown(error) => ("uncertain", error.clone()),
                Err(error) => ("failed", error.clone()),
            };
            self.session
                .get()
                .expect("authority record requires session")
                .store
                .authority_action_finish(&id, state, &outcome)
                .map_err(|e| e.to_string())?;
        }
        result
    }

    /// Publish a review `submit_review` just recorded when an authority grant
    /// covers it. Called by the submission itself, which for a session is the
    /// node's check task once the checks pass rather than this call.
    pub(crate) async fn auto_publish_review(
        &self,
        ctx: &CallContext,
        submitted: Value,
    ) -> Result<Value, String> {
        let Some(review_id) = submitted.get("review_id").and_then(Value::as_str) else {
            return Ok(submitted);
        };
        let access = self.session.get().ok_or("review tools are not available")?;
        let review = access
            .store
            .get_review(review_id)
            .map_err(|e| e.to_string())?
            .ok_or("review disappeared after capture")?;
        if review.kind == crate::store::reports::KIND {
            return Err("standalone narrative reports have no automatic publication path".into());
        }
        // The revision the authority decision below is computed against.
        // Authority is granted for a revision, so the publication is bound to
        // that same one: a resubmission arriving while this runs does not
        // inherit the grant.
        let decided_revision = access
            .store
            .latest_review_revision(review_id)
            .map_err(|e| e.to_string())?
            .map(|revision| revision.id);
        let target: crate::review::publish::Target =
            serde_json::from_str(&review.target).map_err(|e| e.to_string())?;
        let intent =
            crate::authority::revision_intent(access.store.as_ref(), decided_revision.as_deref())?;
        // The prose hash binds everything the forge will show. A revision
        // that asks for nothing beyond a new change described by its title
        // and body hashes exactly as it always has, so existing grants hold.
        let prose = if intent.forge == Default::default() {
            crate::corpus::hash_body(&format!(
                "{}\u{1f}{}",
                review.approved_title(),
                review.approved_body()
            ))
        } else {
            crate::corpus::hash_body(&format!(
                "{}\u{1f}{}\u{1f}{}",
                review.approved_title(),
                review.approved_body(),
                serde_json::to_string(&intent.forge).map_err(|e| e.to_string())?
            ))
        };
        let change = target
            .change
            .as_ref()
            .map(|change| format!(":change:{}", change.number))
            .unwrap_or_default();
        let canonical = format!(
            "publish:{}:{}:{}:{}{change}:prose:{}",
            target.provider, target.project, target.base, target.branch, prose
        );
        let authority_args = serde_json::json!({
            "target": canonical.clone(),
            "revision": review.head_sha.clone(),
            "prose_hash": prose.clone(),
        });
        let mut decision = crate::authority::decide(
            access.store.as_ref(),
            &self.policy.read(),
            &crate::authority::AuthorityQuery {
                channel: &ctx.channel,
                session_id: ctx.session_id(),
                action: crate::authority::PUBLISH,
                target: authority_args["target"].as_str().expect("canonical target"),
                revision: authority_args["revision"].as_str(),
                args: &authority_args,
            },
        )?;
        // Replacing history on a shared branch is never unattended, whatever
        // a grant says: the operator sees what it throws away.
        if intent.rewrite && decision.verdict == Verdict::Allow {
            decision.verdict = Verdict::Ask;
            decision.reason =
                Some("a push that rewrites a change's history always goes to the operator".into());
        }
        if decision.verdict != Verdict::Allow {
            let mut submitted = submitted;
            if let Some(object) = submitted.as_object_mut() {
                object.insert(
                    "publication_authority".into(),
                    serde_json::json!({
                        "action": crate::authority::PUBLISH,
                        "target": canonical,
                        "revision": review.head_sha,
                        "prose_hash": prose,
                        "state": match decision.verdict {
                            Verdict::Allow => "allow",
                            Verdict::Ask => "ask",
                            Verdict::Deny => "deny",
                        },
                        "reason": decision.reason,
                    }),
                );
            }
            return Ok(submitted);
        }
        let operation_id =
            crate::corpus::hash_body(&format!("{canonical}\u{1f}{}", review.head_sha));
        let evidence = serde_json::json!({
            "target": canonical.clone(),
            "revision": review.head_sha.clone(),
            "prose_hash": prose.clone(),
        })
        .to_string();
        if let Some((state, outcome, _)) = access
            .store
            .authority_action_existing(
                crate::authority::PUBLISH,
                &canonical,
                &ctx.channel,
                Some(&review.head_sha),
                &operation_id,
            )
            .map_err(|e| e.to_string())?
        {
            if state == "succeeded" {
                return Ok(serde_json::json!({
                    "review_id": review.id,
                    "state": "reconcile",
                    "authority": {
                        "mode": "automatic",
                        "outcome": "replayed",
                        "published": outcome,
                    },
                }));
            }
            return Err(format!(
                "an identical publication is {state}; reconcile its recorded external outcome before retrying"
            ));
        }
        let action_id = uuid::Uuid::now_v7().to_string();
        if let Err(error) = access
            .store
            .authority_action_begin(&crate::store::ActionBegin {
                id: &action_id,
                grant_id: decision.rule_id.as_deref(),
                action: crate::authority::PUBLISH,
                target: &canonical,
                channel: &ctx.channel,
                session_id: ctx.session_id(),
                revision: Some(&review.head_sha),
                operation_id: Some(&operation_id),
                evidence: &evidence,
            })
        {
            if let Some((state, outcome, _)) = access
                .store
                .authority_action_existing(
                    crate::authority::PUBLISH,
                    &canonical,
                    &ctx.channel,
                    Some(&review.head_sha),
                    &operation_id,
                )
                .map_err(|e| e.to_string())?
            {
                if state == "succeeded" {
                    return Ok(serde_json::json!({
                        "review_id": review.id,
                        "state": "reconcile",
                        "authority": {
                            "mode": "automatic",
                            "outcome": "replayed",
                            "published": outcome,
                        },
                    }));
                }
                return Err(format!(
                    "an identical publication is {state}; reconcile its recorded external outcome before retrying"
                ));
            }
            return Err(error.to_string());
        }
        let recheck = || {
            let current = crate::authority::decide(
                access.store.as_ref(),
                &self.policy.read(),
                &crate::authority::AuthorityQuery {
                    channel: &ctx.channel,
                    session_id: ctx.session_id(),
                    action: crate::authority::PUBLISH,
                    target: &canonical,
                    revision: Some(&review.head_sha),
                    args: &authority_args,
                },
            )?;
            if current.verdict != Verdict::Allow {
                return Err("publication authority was revoked or no longer applies".into());
            }
            access
                .store
                .authority_action_set_grant(&action_id, current.rule_id.as_deref())
                .map_err(|e| e.to_string())
        };
        match crate::authority::publish_review(
            &crate::authority::PublishContext {
                store: access.store.as_ref(),
                manager: &access.manager,
                broker: &self.broker,
                cfg: &self.cfg,
                node_id: &ctx.node_id,
            },
            crate::authority::PublishRequest {
                review: &review,
                title: review.approved_title(),
                body: review.approved_body(),
                require_evidence: true,
                recheck_authority: Some(&recheck),
                decided_revision_id: decided_revision.as_deref(),
                outputs: None,
                recover: false,
                operator: false,
            },
        )
        .await
        {
            Ok(crate::authority::Published {
                url: published,
                outputs,
            }) => {
                access
                    .store
                    .authority_action_finish(&action_id, "succeeded", &published)
                    .map_err(|e| e.to_string())?;
                // Candidate evidence records who authorized publication even
                // when no operator ever saw the card: the automatic caller
                // gets its own decision source, and never the operator's
                // `review_decision` event (that is a human-waiting metric).
                if let Some(revision) = access
                    .store
                    .latest_review_revision(&review.id)
                    .map_err(|e| e.to_string())?
                {
                    access
                        .store
                        .record_review_decision(&crate::store::ReviewDecisionRow {
                            id: uuid::Uuid::now_v7().to_string(),
                            review_id: review.id.clone(),
                            revision_id: revision.id,
                            source: "authority".into(),
                            decision: "approved".into(),
                            reason: None,
                            title: Some(review.approved_title().to_string()),
                            body: Some(review.approved_body().to_string()),
                            patch: None,
                            decided_ms: crate::store::now_ms(),
                            outputs_json: serde_json::to_string(&outputs).ok(),
                        })
                        .map_err(|e| e.to_string())?;
                }
                Ok(serde_json::json!({
                    "review_id": review.id, "state": "approved", "published": published,
                    "authority": { "mode": "automatic", "id": decision.rule_id },
                }))
            }
            Err(error) => {
                // A publication whose outcome could not be established is
                // `uncertain`, not `failed`: the replay guard keeps holding
                // the scope, and nobody is told the change was not opened.
                let outcome = match error {
                    crate::authority::PublishError::Uncertain(_) => "uncertain",
                    _ => "failed",
                };
                let error = error.to_string();
                access
                    .store
                    .authority_action_finish(&action_id, outcome, &error)
                    .map_err(|e| e.to_string())?;
                let state = access
                    .store
                    .get_review(&review.id)
                    .map_err(|e| e.to_string())?
                    .map(|current| current.state)
                    .unwrap_or_else(|| "missing".into());
                Ok(serde_json::json!({
                    "review_id": review.id, "state": state,
                    "authority": { "mode": "automatic", "outcome": outcome, "reason": error },
                }))
            }
        }
    }

    /// The definitions this caller is offered: the channel's tools, as
    /// [`Self::list_for`] narrows them to its phase. A session on a forge
    /// repository is not required to name it to that forge's tools, since the
    /// call defaults to it; an external caller has no repository to default to.
    pub fn list_offered(&self, ctx: &CallContext) -> Vec<Value> {
        let audience = self.audience(ctx);
        let mut all = self.list_for(&ctx.channel, &ctx.node_id, audience);
        if let Audience::Session(phase) = audience {
            let core = Self::always_loaded(phase);
            for tool in &mut all {
                if tool["name"].as_str().is_some_and(|n| core.contains(&n)) {
                    tool["_meta"] = json!({ "anthropic/alwaysLoad": true });
                }
            }
        }
        if !ctx.is_external() {
            if let Some(repo) = self.session_forge_repo(ctx) {
                for tool in &mut all {
                    let defaulted: &[&str] = match tool["name"].as_str() {
                        Some(review::SUBMIT) => &["provider", "project"],
                        Some(name) if forge_of_tool(name) == Some(repo.forge) => &[repo.argument()],
                        _ => &[],
                    };
                    // `get_mut`, not indexing: `tool[..]["required"]` on a
                    // schema without one inserts `null`, and Claude Code
                    // drops every tool of a server offering one.
                    if let Some(required) = tool
                        .get_mut("inputSchema")
                        .and_then(|schema| schema.get_mut("required"))
                        .and_then(Value::as_array_mut)
                    {
                        required.retain(|key| !defaulted.iter().any(|d| key == d));
                    }
                }
            }
        }
        all
    }

    /// The repository the calling session works on, as its launch resolved
    /// it from `origin` into the project row: two row reads, and no git. `None`
    /// for an external caller, and for a session on a local-only repository.
    fn session_forge_repo(&self, ctx: &CallContext) -> Option<crate::forge::ForgeRepo> {
        let id = ctx.session_id()?;
        let remote = self.session.get()?.store.session_remote(id).ok()??;
        crate::forge::ForgeRepo::from_remote(&remote)
    }

    /// The arguments with every default written in: the provider's own, and
    /// the calling session's repository for its forge's tools and for
    /// `submit_review`. Applied before policy, approval, authority and any
    /// hash of the arguments, so a call that leaves a default out and one
    /// that spells it are the same request. `None` when nothing was missing.
    fn with_defaults(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        if name == review::SUBMIT {
            return submission_defaults(args, self.session_forge_repo(ctx).as_ref());
        }
        // Only a tool on the session's own forge can take its repository.
        let repo = forge_of_tool(name).and_then(|forge| {
            self.session_forge_repo(ctx)
                .filter(|repo| repo.forge == forge)
        });
        Ok(with_provider_defaults(name, args, repo.as_ref()))
    }

    /// What the node's own configuration refuses whatever the policy or the
    /// operator says: a workflow `[github] dispatch` does not list, a
    /// `ready` that is not true or false, or a document call that cannot run
    /// (`docs::refusal_before_asking`).
    fn refusal_before_asking(&self, ctx: &CallContext, name: &str, args: &Value) -> Option<String> {
        match name {
            work::WORK_UPDATE => work::refusal_before_asking(self.session.get()?, ctx, args),
            docs::DOC_WRITE | docs::DOC_DELETE => docs::refusal_before_asking(name, args),
            docs::DOC_EDIT => docs::refusal_before_asking(name, args)
                .or_else(|| docs::edit_refusal(&self.session.get()?.store, &ctx.channel, args)),
            github::RUN_DISPATCH => github::dispatch_refusal(&self.cfg.live().github, args),
            // A variable or input naming production is never run, so the
            // operator is not asked about it.
            gitlab::PIPELINE_RUN => gitlab::run_refusal(args),
            // The card names the direction, so a `ready` it cannot read is
            // refused before it is shown.
            github::PR_READY | gitlab::MR_READY => github::ready_arg(args).err(),
            _ => None,
        }
    }

    /// Why a call cannot be asked as it stands: an argument its tool's own
    /// definition requires is missing or null once the defaults are written
    /// in. The operator is not asked to decide a call that could never run
    /// (#874). Arguments the definition does not name are said alongside, as
    /// the likely mistake, but do not refuse a call on their own.
    fn missing_required(&self, ctx: &CallContext, name: &str, args: &Value) -> Option<String> {
        let tool = self
            .list(&ctx.channel, &ctx.node_id)
            .into_iter()
            .find(|t| t["name"] == name)?;
        let schema = tool.get("inputSchema")?;
        let missing: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)?
            .iter()
            .filter_map(Value::as_str)
            .filter(|key| args.get(*key).is_none_or(Value::is_null))
            .collect();
        if missing.is_empty() {
            return None;
        }
        let mut refusal = format!(
            "{name} requires {}, which this call does not give; nothing was asked or run",
            missing.join(", ")
        );
        if let (Some(properties), Some(given)) = (
            schema.get("properties").and_then(Value::as_object),
            args.as_object(),
        ) {
            let unknown: Vec<&str> = given
                .keys()
                .map(String::as_str)
                .filter(|key| !properties.contains_key(*key))
                .collect();
            if !unknown.is_empty() {
                refusal.push_str(&format!(". {name} does not take {}", unknown.join(", ")));
            }
        }
        Some(refusal)
    }

    /// Whose list the caller is read against: its session's phase as the
    /// session's row records it, or the external harness's. A session the
    /// store does not know is read as ordinary work.
    fn audience(&self, ctx: &CallContext) -> Audience {
        use crate::session::Phase;
        let Some(id) = ctx.session_id() else {
            return Audience::External;
        };
        let phase = self
            .session
            .get()
            .and_then(|a| a.store.get_session(id).ok().flatten())
            .map(|s| Phase::from_row(&s.phase))
            .unwrap_or_default();
        Audience::Session(phase)
    }

    fn is_plan_artifact(&self, ctx: &CallContext, args: &Value) -> bool {
        let (Some(access), Some(id)) = (self.session.get(), ctx.session_id()) else {
            return false;
        };
        let Ok(Some(session)) = access.store.get_session(id) else {
            return false;
        };
        session.phase == "plan"
            && session.work_item_id.as_deref().is_some_and(|item| {
                args["slug"].as_str().map(str::trim)
                    == Some(crate::corpus::work::plan_slug(item).as_str())
            })
    }

    /// Policy before the broker. Deny is final and explained; Ask goes to the
    /// queue as a permission request on the calling session and waits for the
    /// operator (or the same expiry every unanswered request gets); Allow
    /// proceeds. A tool the policy does not mention is therefore asked, not
    /// run — adding a tool never widens what runs unattended. Returns the
    /// arguments the operator rewrote on the card, when they did.
    /// Hold a call the operator must decide as an approval and say so. Asking
    /// again for the same thing on the channel while it waits is the same
    /// approval, so a client that retries does not queue a second card.
    async fn request_approval(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
    ) -> Result<Value, String> {
        use crate::store::approvals::{ApprovalRow, PENDING};
        let access = self
            .session
            .get()
            .ok_or("this call needs the operator's approval and this node cannot hold one")?;
        let request_key = request_key(name, args);
        // A plugin's tool is asked about on the build active now, which its
        // card shows and an approved run must still be.
        let build = self.plugin_tool(name).map(|pt| pt.digest);
        let mut existing = access
            .store
            .pending_approval(&ctx.channel, &request_key)
            .map_err(|e| e.to_string())?;
        if let (Some(waiting), Some(build)) = (&existing, &build) {
            let asked_on = access
                .store
                .approval_plugin_digest(&waiting.id)
                .map_err(|e| e.to_string())?;
            if asked_on.as_ref() != Some(build) {
                let reason = format!(
                    "the plugin behind {name} changed build while this waited; the same call \
                     was asked again on the build active now"
                );
                if access
                    .store
                    .expire_approval(&waiting.id, &reason)
                    .map_err(|e| e.to_string())?
                {
                    if let Ok(Some(expired)) = access.store.get_approval(&waiting.id) {
                        access
                            .manager
                            .approval_decided(&expired, crate::store::approvals::EXPIRED)
                            .await;
                    }
                }
                existing = None;
            }
        }
        let approval = match existing {
            Some(approval) => approval,
            None => {
                let title = self.approval_title(ctx, name, args).await;
                let now = crate::store::now_ms();
                let row = ApprovalRow {
                    id: uuid::Uuid::now_v7().to_string(),
                    channel: ctx.channel.clone(),
                    session_id: ctx.session_id().map(str::to_string),
                    node_id: ctx.node_id.clone(),
                    lane: ctx.lane().map(str::to_string),
                    tool: name.to_string(),
                    arguments: args.to_string(),
                    request_key,
                    title,
                    state: PENDING.into(),
                    answer_option_id: None,
                    edited_arguments: None,
                    result: None,
                    reason: None,
                    operator_note: None,
                    claimed_ms: None,
                    created_ms: now,
                    decided_ms: None,
                    finished_ms: None,
                    expires_ms: now
                        + (self.cfg.live().session.approval_expiry_secs.max(1) as i64) * 1000,
                };
                access
                    .store
                    .insert_approval(&row)
                    .map_err(|e| e.to_string())?;
                if let Some(build) = &build {
                    if let Err(e) = access.store.set_approval_plugin_digest(&row.id, build) {
                        // Unrecorded, the card could not be held to its build.
                        let _ = access.store.expire_approval(&row.id, &e.to_string());
                        return Err(e.to_string());
                    }
                }
                access.manager.approval_requested(&row).await;
                row
            }
        };
        Ok(json!({
            "state": "awaiting_operator",
            "approval_id": approval.id,
            "summary": approval.title,
            "message": format!(
                "Nothing has run yet. The operator decides this call; once allowed, the node \
                 runs it and keeps the result. Call {} with this approval_id to wait for the \
                 outcome.",
                approvals::STATUS
            ),
        }))
    }

    /// What the operator reads on the card and in the push for a held call:
    /// [`summarize`], except that a document write says whether it creates
    /// or edits, and what, and when it changes the document's format or
    /// drops files of its bundle. A lookup that fails leaves the plain
    /// summary rather than holding the request. A plugin's tool reads as
    /// [`plugin_title`] until its card shows a preview, whose summary then
    /// titles it.
    async fn approval_title(&self, ctx: &CallContext, name: &str, args: &Value) -> String {
        if name == docs::DOC_EDIT {
            if let Some(access) = self.session.get() {
                if let Ok(current) = docs::replaced(&access.store, &ctx.channel, args) {
                    let mut title = docs::approval_title(&summarize(name, args), current.as_ref());
                    bound(&mut title);
                    return title;
                }
            }
        }
        if name == docs::DOC_WRITE {
            if let Some(access) = self.session.get() {
                if let Ok(current) = docs::replaced(&access.store, &ctx.channel, args) {
                    let mut title = docs::approval_title(&summarize(name, args), current.as_ref());
                    if let Some(consequence) =
                        docs::write_consequence(&access.store, &ctx.channel, args)
                    {
                        title = format!("{title} ({consequence})");
                    }
                    bound(&mut title);
                    return title;
                }
            }
        }
        if let Some(pt) = self.plugin_tool(name) {
            return plugin_title(&pt.plugin, &pt.tool, args);
        }
        // Asked only once `github::merge_head_refusal` read this head as the
        // pull request's own, which the card says.
        if name == github::PR_MERGE {
            let mut title = format!("{}, its current head when asked", summarize(name, args));
            bound(&mut title);
            return title;
        }
        summarize(name, args)
    }

    /// Who an approval's call runs as: the session that asked, or the
    /// channel's external caller. An approval an attached external session
    /// asked for before calls went session-less runs as the channel's
    /// external caller now.
    fn approval_call_context(
        &self,
        access: &SessionAccess,
        approval: &crate::store::approvals::ApprovalRow,
    ) -> CallContext {
        let session = approval.session_id.clone().filter(|id| {
            !access
                .store
                .get_session(id)
                .ok()
                .flatten()
                .is_some_and(|s| s.harness_id == crate::session::external::HARNESS_ID)
        });
        let caller = match session {
            Some(session_id) => Caller::Session(session_id),
            None => Caller::External {
                lane: approval.lane.clone(),
                wait_budget: None,
            },
        };
        CallContext {
            caller,
            channel: approval.channel.clone(),
            node_id: approval.node_id.clone(),
        }
    }

    /// The operator's answer to a plugin's install card (`crate::plugin`):
    /// allowing it makes the build it names the plugin's active one, after
    /// checking it again; anything else drops the build. Recorded on the
    /// card either way.
    async fn answer_plugin_card(
        &self,
        access: &SessionAccess,
        approval: crate::store::approvals::ApprovalRow,
        option_id: &str,
        reason: Option<String>,
        notes: Option<String>,
        by: Option<&str>,
    ) -> Result<(), crate::session::SessionError> {
        use crate::session::SessionError;
        use crate::store::approvals::{Decision, FAILED, REJECTED, RUNNING, SUCCEEDED};
        if approval.node_id != access.manager.node_id() {
            return Err(SessionError::Rejected(
                "this install card belongs to another node".into(),
            ));
        }
        let allow = option_id == OPTION_ALLOW_ONCE;
        let reason = match allow {
            true => None,
            false => Some(reason.unwrap_or_else(|| "the operator did not install it".into())),
        };
        let state = if allow { RUNNING } else { REJECTED };
        let decision = Decision {
            state,
            option_id,
            reason: reason.as_deref(),
            note: notes.as_deref(),
            bound_to: Some(&approval.arguments),
            ..Default::default()
        };
        if !access.store.decide_approval(&approval.id, &decision)? {
            return Err(SessionError::Rejected(
                "this approval is no longer waiting".into(),
            ));
        }
        let decided = access
            .store
            .get_approval(&approval.id)?
            .unwrap_or_else(|| approval.clone());
        access.manager.approval_answered(&decided, state, by).await;
        let store = access.store.clone();
        let card = approval.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            crate::plugin::decide(&store, &Config::state_dir(), &card, allow)
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        if !allow {
            if let Err(error) = outcome {
                tracing::warn!(approval = %approval.id, %error, "a rejected install card left its build");
            }
            return Ok(());
        }
        if outcome.is_ok() {
            // An update: what ran the build before stops, and the next use
            // starts this one.
            if let Ok(name) = crate::plugin::card_plugin(&approval) {
                access.manager.plugins().stop_plugin(&name).await;
            }
        }
        let (state, result, reason) = match outcome {
            Ok(result) => (SUCCEEDED, Some(result.to_string()), None),
            Err(error) => (FAILED, None, Some(error)),
        };
        access
            .store
            .finish_approval(&approval.id, state, result.as_deref(), reason.as_deref())?;
        if let Some(settled) = access.store.get_approval(&approval.id)? {
            access.manager.approval_decided(&settled, state).await;
        }
        Ok(())
    }

    /// The operator's answer to an approval. Allowing it runs the call, with
    /// their edit when they made one, held to every refusal the call itself
    /// would meet. Refusing it, or asking for changes, settles it with what
    /// they said, for the caller to read. The answer returns once the
    /// decision is recorded; the run and its result belong to the approval,
    /// not to whoever answered.
    pub async fn answer_approval(
        self: &Arc<Self>,
        id: &str,
        answer: crate::session::OperatorAnswer,
    ) -> Result<(), crate::session::SessionError> {
        use crate::session::SessionError;
        use crate::store::approvals::{
            Decision, CHANGES_REQUESTED, OPTION_REQUEST_CHANGES, PENDING, REJECTED, RUNNING,
        };
        let rejected = |m: &str| SessionError::Rejected(m.to_string());
        let access = self
            .session
            .get()
            .ok_or_else(|| rejected("approvals are not available"))?;
        let approval = access
            .store
            .get_approval(id)?
            .ok_or_else(|| rejected("no such approval"))?;
        let no_longer_waiting = || rejected("this approval is no longer waiting");
        if approval.state != PENDING {
            return Err(no_longer_waiting());
        }
        // Expired is over even before the sweep marks it so. Checked here,
        // ahead of every answer, because some answers write before they
        // decide (saving an egress host to the repository).
        if approval.expires_ms <= crate::store::now_ms() {
            return Err(rejected("this approval has expired"));
        }
        // The answer names the call it was given for. Arguments that no
        // longer hash to what the operator read are another call, whatever
        // put them there; nothing is decided on it.
        if let Some(read) = answer.arguments_hash.as_deref().map(str::trim) {
            if read != approval.arguments_hash() {
                return Err(rejected(
                    "this approval's arguments are not the ones you read; nothing was decided. \
                     Open it again to see the call as it stands",
                ));
            }
        }
        // The answer names the plugin preview it was given against. A card
        // opened again since (another tab) previewed anew, perhaps against a
        // target that moved: what this operator read is not what would apply.
        let mut preview_token = None;
        if let Some(read) = answer.preview_hash.as_deref().map(str::trim) {
            let kept = access.store.preview_token(id)?;
            if kept.as_deref().map(preview_hash).as_deref() != Some(read) {
                return Err(rejected(
                    "the preview changed since you read it; nothing was decided. \
                     Open it again",
                ));
            }
            preview_token = kept;
        }
        let by = answer.by.clone();
        let said = |text: &Option<String>| {
            text.as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        };
        let (reason, notes) = (said(&answer.reason), said(&answer.notes));
        let option_id = answer.option_id.as_str();
        // An egress card opens or saves the host it asked about and runs no
        // call, so an edit to it would be silently dropped: what the operator
        // allowed would not be what happened. Refused instead.
        let place = crate::store::approvals::PLACE_TOOL;
        let plugin = crate::store::approvals::PLUGIN_INSTALL_TOOL;
        let plugin_card = crate::store::approvals::PLUGIN_CARD_TOOL;
        let takes_no_edit = matches!(approval.tool.as_str(), egress::REQUEST | egress::PREPARE)
            || approval.tool == place
            || approval.tool == plugin
            || approval.tool == plugin_card;
        if takes_no_edit
            && option_id != OPTION_REQUEST_CHANGES
            && option_id != crate::adapter::types::OPTION_REJECT_ONCE
        {
            if let Some(edited) = &answer.arguments {
                let asked: Value = serde_json::from_str(&approval.arguments).unwrap_or_default();
                if *edited != asked {
                    let message = match approval.tool.as_str() {
                        t if t == place => {
                            "this card runs the session where it asked and takes no edit; \
                                 reject it instead"
                        }
                        t if t == plugin => {
                            "a plugin is installed as published, or not: this card takes no \
                                 edit; reject it instead"
                        }
                        t if t == plugin_card => {
                            "a plugin's card is decided as the plugin asked it: it takes no \
                                 edit; ask for changes instead"
                        }
                        _ => {
                            "this card opens the host it asked about and takes no edit; \
                                  reject it instead"
                        }
                    };
                    return Err(SessionError::Unfit(vec![schema::FieldError {
                        field: String::new(),
                        message: message.into(),
                    }]));
                }
            }
        }
        if approval.tool == egress::REQUEST {
            if let Some(opened) = egress::scope_of(option_id) {
                return self
                    .allow_egress(&approval, option_id, opened, notes, by.as_deref())
                    .await;
            }
        }
        // A placement card runs the session there or ends it: there is no
        // call to run, and nothing to change.
        if approval.tool == place {
            if option_id == OPTION_REQUEST_CHANGES {
                return Err(rejected(
                    "a session is run on DigitalOcean or not run: allow it or reject it",
                ));
            }
            if option_id == OPTION_ALLOW_ONCE {
                use crate::store::approvals::{Decision, RUNNING, SUCCEEDED};
                let decision = Decision {
                    state: RUNNING,
                    option_id,
                    note: notes.as_deref(),
                    bound_to: Some(&approval.arguments),
                    ..Default::default()
                };
                if !access.store.decide_approval(id, &decision)? {
                    return Err(no_longer_waiting());
                }
                let decided = access.store.get_approval(id)?.unwrap_or(approval);
                access
                    .manager
                    .approval_answered(&decided, RUNNING, by.as_deref())
                    .await;
                access.store.finish_approval(
                    id,
                    SUCCEEDED,
                    Some(&json!({ "placed": "digitalocean" }).to_string()),
                    None,
                )?;
                return Ok(());
            }
        }
        // A plugin's card runs nothing here: allowing it settles it as
        // approved, for the plugin waiting on `approval/status` to act on.
        // Refusing it, or asking for changes, is answered as any call's is,
        // below.
        if approval.tool == plugin_card && option_id == OPTION_ALLOW_ONCE {
            use crate::store::approvals::SUCCEEDED;
            let decision = Decision {
                state: RUNNING,
                option_id,
                note: notes.as_deref(),
                bound_to: Some(&approval.arguments),
                ..Default::default()
            };
            if !access.store.decide_approval(id, &decision)? {
                return Err(no_longer_waiting());
            }
            let decided = access.store.get_approval(id)?.unwrap_or(approval);
            access
                .manager
                .approval_answered(&decided, RUNNING, by.as_deref())
                .await;
            access.store.finish_approval(
                id,
                SUCCEEDED,
                Some(&json!({ "approved": true }).to_string()),
                None,
            )?;
            if let Some(settled) = access.store.get_approval(id)? {
                access.manager.approval_decided(&settled, SUCCEEDED).await;
            }
            return Ok(());
        }
        // An install card installs the build it names, or drops it: there is
        // no call to run, and nothing to change.
        if approval.tool == plugin {
            if option_id == OPTION_REQUEST_CHANGES {
                return Err(rejected(
                    "a plugin is installed as published, or not: allow it or reject it",
                ));
            }
            return self
                .answer_plugin_card(access, approval, option_id, reason, notes, by.as_deref())
                .await;
        }
        // A warm-up's ask is saved or refused: the warm-up is over, so there
        // is nothing to open once, and no call to run or to change.
        if approval.tool == egress::PREPARE {
            if option_id == crate::store::approvals::OPTION_ALLOW_REPO {
                return self
                    .save_warming_host(&approval, option_id, notes, by.as_deref())
                    .await;
            }
            if option_id == OPTION_ALLOW_ONCE || option_id == OPTION_REQUEST_CHANGES {
                return Err(rejected(
                    "a warm-up's host is saved to the repository's egress or rejected",
                ));
            }
        }
        if option_id != OPTION_ALLOW_ONCE {
            let (state, reason, note) = if option_id == OPTION_REQUEST_CHANGES {
                let Some(notes) = notes.or(reason) else {
                    return Err(SessionError::Unfit(vec![schema::FieldError {
                        field: "notes".into(),
                        message: "say what should change".into(),
                    }]));
                };
                (
                    CHANGES_REQUESTED,
                    "the operator asked for changes".to_string(),
                    Some(notes),
                )
            } else {
                (
                    REJECTED,
                    reason.unwrap_or_else(|| "the operator did not allow this call".into()),
                    notes,
                )
            };
            let decision = Decision {
                state,
                option_id,
                reason: Some(&reason),
                note: note.as_deref(),
                bound_to: Some(&approval.arguments),
                ..Default::default()
            };
            if !access.store.decide_approval(id, &decision)? {
                return Err(no_longer_waiting());
            }
            let decided = access.store.get_approval(id)?.unwrap_or(approval);
            access
                .manager
                .approval_answered(&decided, state, by.as_deref())
                .await;
            return Ok(());
        }
        let original: Value = serde_json::from_str(&approval.arguments)
            .map_err(|e| SessionError::Rejected(e.to_string()))?;
        if let Some(edited) = &answer.arguments {
            let errors = schema::check_edit(&approval.tool, &original, edited);
            if !errors.is_empty() {
                return Err(SessionError::Unfit(errors));
            }
        }
        // A plugin's tool whose entry requires it is allowed only once a
        // preview of the call as it would run, the edit included, has been
        // shown. Held here, not only on the page, so an answer from anywhere
        // meets it; refusing or asking for changes never needs one.
        let needs_preview = match self.plugin_tool(&approval.tool) {
            Some(pt) if pt.requires_preview() => {
                let runs = answer.arguments.as_ref().unwrap_or(&original);
                let want = crate::store::approvals::arguments_hash(&runs.to_string());
                if access.store.preview_shown_for(id)?.as_deref() != Some(want.as_str()) {
                    return Err(rejected(&no_preview_shown(&pt.plugin)));
                }
                Some((pt.plugin, want))
            }
            _ => None,
        };
        let ctx = self.approval_call_context(access, &approval);
        // What runs is the operator's rewrite, and a refusal is final whoever
        // wrote the words.
        let edited = answer.arguments.as_ref().map(Value::to_string);
        let run_args = answer.arguments.unwrap_or(original);
        // What the operator read was a preview taken when the card opened.
        // Taken again now, so a target that has moved since (another card
        // changed the same row) is refused here, before anything is
        // decided, and the page shows the fresh preview. A preview that
        // cannot be taken now leaves it to the plugin's own check at
        // dispatch, which also catches a change that lands after this.
        if let Some(kept) = preview_token.as_deref() {
            if let Some(pt) = self.plugin_tool(&approval.tool) {
                if let Ok(Some(fresh)) = self.ask_preview(access, &approval, &pt, &run_args).await {
                    let token = preview_shape(&fresh).ok().and_then(|p| p.token);
                    if token.is_some_and(|token| token != kept) {
                        return Err(SessionError::Stale(
                            "what this call changes moved since you read its preview; nothing \
                             was decided. Review the fresh preview and answer again"
                                .into(),
                        ));
                    }
                }
            }
        }
        // Bound to the arguments read above, which are what runs unless the
        // operator rewrote them: a row changed in between is not decided.
        let decision = Decision {
            state: RUNNING,
            option_id,
            edited_arguments: edited.as_deref(),
            note: notes.as_deref(),
            bound_to: Some(&approval.arguments),
            preview_token: preview_token.as_deref(),
            preview_shown_for: needs_preview.as_ref().map(|(_, want)| want.as_str()),
            ..Default::default()
        };
        if !access.store.decide_approval(id, &decision)? {
            if let Some((plugin, want)) = &needs_preview {
                // Still waiting, but no longer previewed for this call: the
                // card was previewed again since (another tab), and failed.
                let still = access
                    .store
                    .get_approval(id)?
                    .is_some_and(|a| a.state == PENDING);
                if still && access.store.preview_shown_for(id)?.as_deref() != Some(want.as_str()) {
                    return Err(rejected(&no_preview_shown(plugin)));
                }
            }
            return Err(no_longer_waiting());
        }
        let decided = access.store.get_approval(id)?.unwrap_or(approval);
        access
            .manager
            .approval_answered(&decided, RUNNING, by.as_deref())
            .await;
        let tools = Arc::clone(self);
        let name = decided.tool.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let outcome = tools.run_approved(&ctx, &name, &run_args, &id).await;
            tools.settle_approval(&id, outcome).await;
        });
        Ok(())
    }

    /// Allow an egress ask: open its host to the asking session's own grant,
    /// for as long as the operator said, and record what was opened. Saving
    /// it to the repository is checked before anything is decided, so an
    /// entry that cannot take it leaves the card waiting with the reason.
    async fn allow_egress(
        &self,
        approval: &crate::store::approvals::ApprovalRow,
        option_id: &str,
        opened: egress::Opened,
        notes: Option<String>,
        by: Option<&str>,
    ) -> Result<(), crate::session::SessionError> {
        use crate::session::SessionError;
        use crate::store::approvals::{Decision, FAILED, RUNNING, SUCCEEDED};
        let access = self
            .session
            .get()
            .ok_or_else(|| SessionError::Rejected("approvals are not available".into()))?;
        let host = egress::host_of(approval).map_err(SessionError::Rejected)?;
        let session_id = approval
            .session_id
            .clone()
            .ok_or_else(|| SessionError::Rejected("this ask names no session".into()))?;
        let saved = if opened.scope == "repository" {
            Some(
                crate::environment::save_session_host(&self.cfg, &access.store, &session_id, &host)
                    .map_err(SessionError::Rejected)?,
            )
        } else {
            None
        };
        let decision = Decision {
            state: RUNNING,
            option_id,
            note: notes.as_deref(),
            bound_to: Some(&approval.arguments),
            ..Default::default()
        };
        if !access.store.decide_approval(&approval.id, &decision)? {
            return Err(SessionError::Rejected(
                "this approval is no longer waiting".into(),
            ));
        }
        let decided = access.store.get_approval(&approval.id)?;
        access
            .manager
            .approval_answered(decided.as_ref().unwrap_or(approval), RUNNING, by)
            .await;
        let widened = access
            .manager
            .egress_grants()
            .is_some_and(|grants| grants.widen_session(&session_id, &host, opened.until));
        let (state, result, reason) = if widened {
            let result = json!({
                "host": host,
                "scope": opened.scope,
                "for_secs": opened.until.map(|_| egress::ONCE.as_secs()),
                "repository": saved,
            });
            (SUCCEEDED, Some(result.to_string()), None)
        } else {
            (
                FAILED,
                None,
                Some(match &saved {
                    Some(repo) => format!(
                        "the session holds no grant any more, so nothing was opened now; \
                         {host} was saved to {repo}'s egress for its next session"
                    ),
                    None => "the session holds no grant any more; nothing was opened".into(),
                }),
            )
        };
        access
            .store
            .finish_approval(&approval.id, state, result.as_deref(), reason.as_deref())?;
        if let Some(settled) = access.store.get_approval(&approval.id)? {
            access.manager.approval_decided(&settled, state).await;
        }
        Ok(())
    }

    /// Allow a warm-up's ask: save its host to the repository's egress, then
    /// warm that repository's base cache again in the background. Saving is
    /// done before anything is decided, so an entry that cannot take it
    /// leaves the card waiting with the reason.
    async fn save_warming_host(
        &self,
        approval: &crate::store::approvals::ApprovalRow,
        option_id: &str,
        notes: Option<String>,
        by: Option<&str>,
    ) -> Result<(), crate::session::SessionError> {
        use crate::session::SessionError;
        use crate::store::approvals::{Decision, RUNNING, SUCCEEDED};
        let access = self
            .session
            .get()
            .ok_or_else(|| SessionError::Rejected("approvals are not available".into()))?;
        let host = egress::host_of(approval).map_err(SessionError::Rejected)?;
        let repo = egress::repo_of(approval).map_err(SessionError::Rejected)?;
        let saved =
            crate::environment::save_preparation_host(&self.cfg, &access.store, &repo, &host)
                .map_err(SessionError::Rejected)?;
        let decision = Decision {
            state: RUNNING,
            option_id,
            note: notes.as_deref(),
            bound_to: Some(&approval.arguments),
            ..Default::default()
        };
        if !access.store.decide_approval(&approval.id, &decision)? {
            return Err(SessionError::Rejected(
                "this approval is no longer waiting".into(),
            ));
        }
        let decided = access.store.get_approval(&approval.id)?;
        access
            .manager
            .approval_answered(decided.as_ref().unwrap_or(approval), RUNNING, by)
            .await;
        let result = json!({ "host": host, "scope": "repository", "repository": saved });
        access
            .store
            .finish_approval(&approval.id, SUCCEEDED, Some(&result.to_string()), None)?;
        if let Some(settled) = access.store.get_approval(&approval.id)? {
            access.manager.approval_decided(&settled, SUCCEEDED).await;
        }
        let manager = access.manager.clone();
        tokio::spawn(async move {
            crate::repo_image::warm_base_cache(
                manager.backend().as_ref(),
                manager.cfg(),
                manager.store(),
                &repo,
            )
            .await;
        });
        Ok(())
    }

    async fn run_approved(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
        approval: &str,
    ) -> Result<Value, String> {
        let decision = self.decide(ctx, name, args);
        if decision.verdict == Verdict::Deny {
            return Err(refusal(decision));
        }
        let access = self.session.get().ok_or("approvals are not available")?;
        access
            .manager
            .approval_runnable(ctx)
            .map_err(|e| e.to_string())?;
        self.dispatch(ctx, name, args, Some(approval)).await
    }

    async fn settle_approval(&self, id: &str, outcome: Result<Value, String>) {
        use crate::store::approvals::{FAILED, SUCCEEDED, UNCERTAIN};
        let Some(access) = self.session.get() else {
            return;
        };
        let (state, result, reason) = match &outcome {
            Ok(value) => (SUCCEEDED, Some(value.to_string()), None),
            // The plugin applied against what the card showed and found it
            // moved: nothing was sent, so the call failed, and is called again.
            Err(error) if error.starts_with(upstream::STALE) => {
                let tool = access
                    .store
                    .get_approval(id)
                    .ok()
                    .flatten()
                    .map(|a| a.tool)
                    .unwrap_or_else(|| "the tool".into());
                let said = &error[upstream::STALE.len()..];
                (
                    FAILED,
                    None,
                    Some(format!(
                        "{}{said}. What the card showed has changed since; nothing was sent. \
                         Call {tool} again for a fresh card.",
                        crate::store::approvals::STALE_REASON
                    )),
                )
            }
            Err(error) if remote_outcome_unknown(error) => (UNCERTAIN, None, Some(error.clone())),
            Err(error) => (FAILED, None, Some(error.clone())),
        };
        if let Err(e) =
            access
                .store
                .finish_approval(id, state, result.as_deref(), reason.as_deref())
        {
            tracing::error!(error = %e, approval = id, "failed to record an approval's outcome");
        }
        if let Ok(Some(approval)) = access.store.get_approval(id) {
            access.manager.approval_decided(&approval, state).await;
        }
    }

    fn decide(&self, ctx: &CallContext, name: &str, args: &Value) -> Decision {
        let live = self.cfg.live();
        let tool = self.policy.read().decide(&Request {
            channel: &ctx.channel,
            action: name,
            kind: Some(TOOL_KIND),
            resource: None,
            command: None,
            arguments: Some(&self.policy_arguments(name, args)),
            doc_kinds: Some(&live.docs.kind),
        });
        if tool.verdict == Verdict::Deny {
            return tool;
        }
        if let Some(pt) = self.plugin_tool(name) {
            // What the plugin says the tool does decides nothing.
            let entry = upstream::Entry::plugin(&pt.plugin, pt.entry.as_ref());
            let decision = upstream::decide(&entry, &pt.tool, &self.policy.read(), tool);
            // A consequential tool is never allowed by name, by the bundle or
            // by the operator's entry: only a grant scoped to this one call
            // allows it. A denial, and an entry that asks, still hold.
            if !pt.consequential()
                || decision.verdict == Verdict::Deny
                || entry.setting(&pt.tool) == Some(Verdict::Ask)
            {
                return decision;
            }
            return self.authority_decision(ctx, plugin_consequential(name, args), args);
        }
        if let Some((server, remote)) = self.upstream_tool(name) {
            return upstream::decide(
                &upstream::Entry::server(&server),
                &remote,
                &self.policy.read(),
                tool,
            );
        }
        if let Some(scope) = consequential(name, args) {
            return self.authority_decision(ctx, Some(scope), args);
        }
        tool
    }

    /// How a consequential call in `scope` is decided: by the grants that
    /// name it, under the signed policy. No scope, or no store to read the
    /// grants from, asks.
    fn authority_decision(
        &self,
        ctx: &CallContext,
        scope: Option<(&'static str, String, Option<String>)>,
        args: &Value,
    ) -> Decision {
        let ask = Decision {
            verdict: Verdict::Ask,
            rule_id: None,
            reason: None,
        };
        let (Some((action, target, revision)), Some(access)) = (scope, self.session.get()) else {
            return ask;
        };
        crate::authority::decide(
            access.store.as_ref(),
            &self.policy.read(),
            &crate::authority::AuthorityQuery {
                channel: &ctx.channel,
                session_id: ctx.session_id(),
                action,
                target: &target,
                revision: revision.as_deref(),
                args,
            },
        )
        .unwrap_or(ask)
    }

    /// Whether a call to `name` is consequential: one of the node's merge or
    /// deploy verbs, or a plugin tool its manifest marks so.
    fn is_consequential(&self, name: &str) -> bool {
        consequential_name(name) || self.plugin_tool(name).is_some_and(|pt| pt.consequential())
    }

    /// The authority scope of a consequential call, `(action, target,
    /// revision)`; `None` for any other call, and for one whose arguments do
    /// not name its scope or carry a valid `operation_id`.
    fn consequential_scope(
        &self,
        name: &str,
        args: &Value,
    ) -> Option<(&'static str, String, Option<String>)> {
        if consequential_name(name) {
            return consequential(name, args);
        }
        self.plugin_tool(name)
            .filter(PluginTool::consequential)
            .and_then(|_| plugin_consequential(name, args))
    }

    /// What a consequential call is recorded as, and compared with on replay.
    fn consequential_payload(&self, name: &str, args: &Value) -> Option<Value> {
        if consequential_name(name) {
            return canonical_consequential_payload(name, args);
        }
        self.plugin_tool(name)
            .filter(PluginTool::consequential)
            .and_then(|_| plugin_consequential_payload(args))
    }

    fn revalidate_tool_policy(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
    ) -> Result<(), String> {
        let live = self.cfg.live();
        let decision = self.policy.read().decide(&Request {
            channel: &ctx.channel,
            action: name,
            kind: Some(TOOL_KIND),
            resource: None,
            command: None,
            arguments: Some(&self.policy_arguments(name, args)),
            doc_kinds: Some(&live.docs.kind),
        });
        if decision.verdict == Verdict::Deny {
            return Err(refusal(decision));
        }
        Ok(())
    }

    /// A call's arguments as the policy matches them. For a plugin's tool,
    /// without the prose its schema marks (`x-tracon-format`): text the call
    /// carries, not an action it takes, as the node's own comment and
    /// document tools have theirs (`policy::prose_fields`), so a comment may
    /// say what a denial names and is read on its card instead. What the call
    /// acts on is still matched.
    fn policy_arguments<'a>(&self, name: &str, args: &'a Value) -> std::borrow::Cow<'a, Value> {
        let Some(shown) = self.plugin_presentation(name) else {
            return std::borrow::Cow::Borrowed(args);
        };
        let strip = |value: &mut Value| {
            if let Some(fields) = value.as_object_mut() {
                for field in &shown.prose_fields {
                    fields.remove(field.as_ref());
                }
            }
        };
        let mut matched = args.clone();
        if let Some(Value::Array(items)) = shown
            .item_list
            .as_deref()
            .and_then(|list| matched.get_mut(list))
        {
            items.iter_mut().for_each(strip);
        }
        strip(&mut matched);
        std::borrow::Cow::Owned(matched)
    }

    /// Grants are re-read after every operator wait and immediately before a
    /// consequential request. Revocation and expiry never rely on an earlier
    /// cached decision.
    fn revalidate_consequential(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
        one_shot: bool,
    ) -> Result<(), String> {
        self.revalidate_tool_policy(ctx, name, args)?;
        let Some((action, target, revision)) = self.consequential_scope(name, args) else {
            return Ok(());
        };
        let decision = crate::authority::decide(
            self.session
                .get()
                .ok_or("authority requires a session")?
                .store
                .as_ref(),
            &self.policy.read(),
            &crate::authority::AuthorityQuery {
                channel: &ctx.channel,
                session_id: ctx.session_id(),
                action,
                target: &target,
                revision: revision.as_deref(),
                args,
            },
        )?;
        match decision.verdict {
            Verdict::Allow => Ok(()),
            Verdict::Deny => Err(refusal(decision)),
            Verdict::Ask if one_shot => Ok(()),
            Verdict::Ask => Err(format!(
                "{action} for {target} needs a current scoped authority grant"
            )),
        }
    }

    fn begin_consequential(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
        one_shot: bool,
    ) -> Result<Option<ActionRecord>, String> {
        self.revalidate_tool_policy(ctx, name, args)?;
        let Some((action, target, revision)) = self.consequential_scope(name, args) else {
            return Ok(None);
        };
        let access = self.session.get().ok_or("authority requires a session")?;
        let decision = crate::authority::decide(
            access.store.as_ref(),
            &self.policy.read(),
            &crate::authority::AuthorityQuery {
                channel: &ctx.channel,
                session_id: ctx.session_id(),
                action,
                target: &target,
                revision: revision.as_deref(),
                args,
            },
        )?;
        if decision.verdict != Verdict::Allow && !(one_shot && decision.verdict == Verdict::Ask) {
            return Err("authority changed before dispatch".into());
        }
        let evidence = serde_json::json!({
            "arguments": self.consequential_payload(name, args).expect("validated consequential arguments"),
        }).to_string();
        let operation_id = args["operation_id"]
            .as_str()
            .expect("validated operation id")
            .trim();
        let request_hash = crate::corpus::hash_body(&evidence);
        if let Some((state, outcome, recorded_hash)) = access
            .store
            .authority_action_existing(
                action,
                &target,
                &ctx.channel,
                revision.as_deref(),
                operation_id,
            )
            .map_err(|e| e.to_string())?
        {
            if recorded_hash != request_hash {
                return Err("operation_id was already used for a different request".into());
            }
            if state == "succeeded" {
                let outcome = outcome.unwrap_or_default();
                return Ok(Some(ActionRecord::Replay(
                    serde_json::from_str(&outcome).unwrap_or(serde_json::json!(outcome)),
                )));
            }
            return Err(format!(
                "this operation is {state}; reconcile its recorded external outcome before retrying"
            ));
        }
        let id = uuid::Uuid::now_v7().to_string();
        if let Err(error) = access
            .store
            .authority_action_begin(&crate::store::ActionBegin {
                id: &id,
                grant_id: decision.rule_id.as_deref(),
                action,
                target: &target,
                channel: &ctx.channel,
                session_id: ctx.session_id(),
                revision: revision.as_deref(),
                operation_id: Some(operation_id),
                evidence: &evidence,
            })
        {
            if let Some((state, outcome, recorded_hash)) = access
                .store
                .authority_action_existing(
                    action,
                    &target,
                    &ctx.channel,
                    revision.as_deref(),
                    operation_id,
                )
                .map_err(|e| e.to_string())?
            {
                if recorded_hash != request_hash {
                    return Err("operation_id was already used for a different request".into());
                }
                if state == "succeeded" {
                    let outcome = outcome.unwrap_or_default();
                    return Ok(Some(ActionRecord::Replay(
                        serde_json::from_str(&outcome).unwrap_or(serde_json::json!(outcome)),
                    )));
                }
                return Err(format!(
                    "this operation is {state}; reconcile its recorded external outcome before retrying"
                ));
            }
            return Err(error.to_string());
        }
        Ok(Some(ActionRecord::New(id)))
    }

    /// Handle one MCP JSON-RPC message. Returns `None` for notifications.
    pub async fn handle(&self, ctx: &CallContext, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned()?;
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => match negotiate_protocol(&params) {
                Ok(version) => Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "tracon", "version": env!("CARGO_PKG_VERSION") },
                })),
                Err(e) => Err(e),
            },
            "tools/list" => {
                self.refresh_upstream(&ctx.node_id, false).await;
                // Plan and review sessions are offered no plugin's tools, so
                // none is started for them.
                if matches!(
                    self.audience(ctx),
                    Audience::Session(crate::session::Phase::Execute) | Audience::External
                ) {
                    self.refresh_plugins(&ctx.channel, &ctx.node_id).await;
                }
                Ok(json!({ "tools": self.list_offered(ctx) }))
            }
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let upstream =
                    self.upstream_tool(name).is_some() || self.plugin_tool(name).is_some();
                self.call(ctx, name, &args)
                    .await
                    .map(|v| match v {
                        // Another server's own content, as it sent it: text,
                        // images or structured results alike.
                        v if upstream && v["content"].is_array() => {
                            let mut v = v;
                            v["isError"] = json!(false);
                            v
                        }
                        v => tool_result(&v, false),
                    })
                    .or_else(|e| Ok(tool_result(&json!(e), true)))
            }
            "ping" => Ok(json!({})),
            other => Err(format!("unsupported method {other}")),
        };
        Some(match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err(e) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": e } })
            }
        })
    }
}

/// The revision an `initialize` asks for, refused by name when it is not one
/// this server serves. A client that names none gets this server's own.
fn negotiate_protocol(params: &Value) -> Result<String, String> {
    let Some(asked) = params.get("protocolVersion").and_then(Value::as_str) else {
        return Ok(PROTOCOL_VERSION.to_string());
    };
    if SUPPORTED_PROTOCOL_VERSIONS.contains(&asked) {
        return Ok(asked.to_string());
    }
    Err(format!(
        "client announces MCP protocol {asked}; this node supports {}",
        SUPPORTED_PROTOCOL_VERSIONS.join(", ")
    ))
}

/// The node's own consequential verbs, which carry their scope in
/// arguments. A plugin's are the tools its manifest marks `consequential`
/// ([`plugin_consequential`]). This parser is strict: malformed input cannot
/// fall through to an unscoped authority decision.
fn consequential_name(name: &str) -> bool {
    matches!(name, github::PR_MERGE | gitlab::MR_MERGE | gitlab::DEPLOY)
}

/// A consequential call's `operation_id`, when it is one: at least 8
/// letters, digits or dashes.
fn operation_id(args: &Value) -> Option<&str> {
    args.get(OPERATION_ID)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| id.len() >= 8 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// Why a consequential call has no scope, naming the field at fault: the
/// `operation_id` and its pattern when that is what is wrong, otherwise the
/// arguments that name what the call acts on.
fn consequential_refusal(name: &str, args: &Value) -> String {
    if operation_id(args).is_none() {
        return match args.get(OPERATION_ID) {
            None | Some(Value::Null) => format!(
                "{name} requires an operation_id: at least 8 ASCII letters, digits or dashes \
                 ({OPERATION_ID_PATTERN})"
            ),
            Some(Value::String(given)) => format!(
                "{name} refused operation_id {given:?}: it must be at least 8 ASCII letters, \
                 digits or dashes, nothing else ({OPERATION_ID_PATTERN}); a dot, underscore or \
                 space is not allowed"
            ),
            Some(_) => format!(
                "{name} refused operation_id: it must be a string of at least 8 ASCII letters, \
                 digits or dashes ({OPERATION_ID_PATTERN})"
            ),
        };
    }
    let fields: &[&str] = match name {
        github::PR_MERGE => &["repo (owner/name)", "number (integer)"],
        gitlab::MR_MERGE => &["project", "iid (integer)"],
        gitlab::DEPLOY => &[
            "project",
            "environment",
            "pipeline_id (integer)",
            "job_id (integer)",
        ],
        _ => &[],
    };
    if fields.is_empty() {
        format!("{name} refused its arguments: they must be a JSON object")
    } else {
        format!(
            "{name} refused its arguments: {} must each be given, and strings not blank",
            fields.join(", ")
        )
    }
}

fn consequential(name: &str, args: &Value) -> Option<(&'static str, String, Option<String>)> {
    let string = |key| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    if consequential_name(name) && operation_id(args).is_none() {
        return None;
    }
    match name {
        github::PR_MERGE => Some((
            crate::authority::MERGE,
            format!(
                "github:{}:pr:{}",
                string("repo")?.to_ascii_lowercase(),
                args.get("number").and_then(Value::as_i64)?,
            ),
            string("head_sha").map(str::to_string),
        )),
        gitlab::MR_MERGE => Some((
            crate::authority::MERGE,
            format!(
                "gitlab:{}:mr:{}",
                string("project")?,
                args.get("iid").and_then(Value::as_i64)?
            ),
            string("head_sha").map(str::to_string),
        )),
        gitlab::DEPLOY => Some((
            crate::authority::DEPLOY,
            format!(
                "gitlab:{}:environment:{}:pipeline:{}:job:{}",
                string("project")?,
                string("environment")?,
                args.get("pipeline_id").and_then(Value::as_i64)?,
                args.get("job_id").and_then(Value::as_i64)?,
            ),
            string("source_sha").map(str::to_string),
        )),
        _ => None,
    }
}

/// The scope of a call to a plugin's tool its manifest marks
/// `consequential`: that one call. The node cannot read a plugin's
/// arguments for what they act on, so the target is the tool and a hash of
/// its arguments (`plugin:<plugin>__<tool>:<sha256>`, the `operation_id`
/// left out): a grant names exactly the call it allows, and no grant covers
/// the tool as a whole. `None` without a valid `operation_id`.
fn plugin_consequential(
    name: &str,
    args: &Value,
) -> Option<(&'static str, String, Option<String>)> {
    let payload = plugin_consequential_payload(args)?;
    let mut call = payload;
    call.as_object_mut()?.remove(OPERATION_ID);
    Some((
        crate::authority::PLUGIN_CALL,
        format!(
            "plugin:{name}:{}",
            crate::corpus::hash_body(&call.to_string())
        ),
        None,
    ))
}

/// A consequential plugin call's arguments as they are recorded and
/// compared on replay: as sent, with the `operation_id` trimmed.
fn plugin_consequential_payload(args: &Value) -> Option<Value> {
    let id = operation_id(args)?.to_string();
    let mut payload = args.as_object()?.clone();
    payload.insert(OPERATION_ID.into(), Value::from(id));
    Some(Value::Object(payload))
}

/// The forge a tool reaches, for the tools that take a repository.
fn forge_of_tool(name: &str) -> Option<crate::forge::Forge> {
    if github::TOOLS.contains(&name) {
        Some(crate::forge::Forge::Github)
    } else if gitlab::TOOLS.contains(&name) {
        Some(crate::forge::Forge::Gitlab)
    } else {
        None
    }
}

/// The arguments with the provider's defaults written in, so the permission
/// prompt, the authority match and the replay record all show what will be
/// sent rather than leaving it to the provider call. `repo` is the calling
/// session's own repository, already matched to the tool's forge, and is the
/// default for the tool's `repo` or `project`; written in here, before
/// anything hashes the arguments, a retried call that names it and one that
/// leaves it out are the same request. `None` when nothing was missing.
fn with_provider_defaults(
    name: &str,
    args: &Value,
    repo: Option<&crate::forge::ForgeRepo>,
) -> Option<Value> {
    let mut defaults = Vec::new();
    match name {
        github::PR_MERGE => defaults.push(("method", Value::from("squash"))),
        gitlab::MR_MERGE => defaults.push(("squash", Value::from(true))),
        _ => {}
    }
    if let Some(repo) = repo {
        defaults.push((repo.argument(), Value::from(repo.path.as_str())));
    }
    let map = args.as_object()?;
    // A blank repository names nothing, so it is left out as surely as an
    // absent one; a blank `method` keeps the meaning it always had.
    let absent = |key: &str| match map.get(key) {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => repo.is_some_and(|r| r.argument() == key) && s.trim().is_empty(),
        Some(_) => false,
    };
    let missing: Vec<_> = defaults
        .into_iter()
        .filter(|(key, _)| absent(key))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let mut map = map.clone();
    for (key, default) in missing {
        map.insert(key.into(), default);
    }
    Some(Value::Object(map))
}

/// `submit_review`'s `provider` and `project`, from the calling session's
/// repository when it has one. Both missing: both filled. One given: the
/// other is filled only when the given one is the session's own; otherwise
/// the call is refused rather than publishing to a pair nobody named. Both
/// given: untouched, wherever they point.
fn submission_defaults(
    args: &Value,
    repo: Option<&crate::forge::ForgeRepo>,
) -> Result<Option<Value>, String> {
    let (Some(repo), Some(map)) = (repo, args.as_object()) else {
        return Ok(None);
    };
    let given = |key: &str| {
        map.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let provider = repo.forge.name();
    let mismatch = |named: String| {
        format!(
            "this session's repository is {provider} `{}`, and the submission names {named}; \
             leave out both provider and project to publish to the session's repository, or \
             give both to publish elsewhere",
            repo.path
        )
    };
    match (given("provider"), given("project")) {
        (Some(_), Some(_)) => return Ok(None),
        (Some(asked), None) if crate::forge::Forge::parse(asked) != Some(repo.forge) => {
            return Err(mismatch(format!("provider `{asked}`")))
        }
        (None, Some(asked)) if !asked.eq_ignore_ascii_case(&repo.path) => {
            return Err(mismatch(format!("project `{asked}`")))
        }
        _ => {}
    }
    let mut filled = map.clone();
    if given("provider").is_none() {
        filled.insert("provider".into(), Value::from(provider));
    }
    if given("project").is_none() {
        filled.insert("project".into(), Value::from(repo.path.as_str()));
    }
    Ok(Some(Value::Object(filled)))
}

/// Normalize provider defaults and ignore arguments a consequential provider
/// does not consume before comparing a stable operation id on replay.
fn canonical_consequential_payload(name: &str, args: &Value) -> Option<Value> {
    let string = |key| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let operation_id = string("operation_id")?;
    match name {
        github::PR_MERGE => Some(serde_json::json!({
            "repo": string("repo")?.to_ascii_lowercase(), "number": args.get("number")?.as_i64()?,
            "head_sha": string("head_sha")?, "method": string("method").unwrap_or("squash"),
            "operation_id": operation_id,
        })),
        gitlab::MR_MERGE => Some(serde_json::json!({
            "project": string("project")?, "iid": args.get("iid")?.as_i64()?,
            "head_sha": string("head_sha")?, "squash": args.get("squash").and_then(Value::as_bool).unwrap_or(true),
            "operation_id": operation_id,
        })),
        gitlab::DEPLOY => Some(serde_json::json!({
            "project": string("project")?, "pipeline_id": args.get("pipeline_id")?.as_i64()?,
            "job_id": args.get("job_id")?.as_i64()?, "environment": string("environment")?,
            "source_sha": string("source_sha")?, "operation_id": operation_id,
        })),
        _ => None,
    }
}

/// What makes two asks the same request on a channel: the tool and its
/// canonical arguments. A consequential call is keyed on the payload its
/// replay record would compare; anything else on its arguments, whose object
/// keys serialize in order.
fn request_key(name: &str, args: &Value) -> String {
    let payload = canonical_consequential_payload(name, args).unwrap_or_else(|| args.clone());
    crate::corpus::hash_body(&format!("{name}\u{1f}{payload}"))
}

/// What a plugin's `tracon/preview` answered, as the contract allows it.
struct PluginPreview<'a> {
    previewable: bool,
    summary: &'a str,
    body: &'a str,
    token: Option<&'a str>,
}

/// `result` read as a preview, or why it is not one.
fn preview_shape(result: &Value) -> Result<PluginPreview<'_>, String> {
    let previewable = result["previewable"]
        .as_bool()
        .ok_or("previewable is not a boolean")?;
    let summary = result["summary"]
        .as_str()
        .ok_or("summary is not a string")?;
    let body = result["body"].as_str().ok_or("body is not a string")?;
    if body.len() > PREVIEW_BODY_MAX {
        return Err(format!("body is larger than {PREVIEW_BODY_MAX} bytes"));
    }
    let token = match &result["token"] {
        Value::Null => None,
        Value::String(t) if t.len() <= PREVIEW_TOKEN_MAX => Some(t.as_str()),
        Value::String(_) => return Err(format!("token is larger than {PREVIEW_TOKEN_MAX} bytes")),
        _ => return Err("token is neither a string nor null".into()),
    };
    Ok(PluginPreview {
        previewable,
        summary,
        body,
        // A call it cannot preview has nothing to apply against.
        token: token.filter(|_| previewable),
    })
}

/// What a plugin tool's card says when Allow is held for want of a preview:
/// the plugin answered none, or its preview failed.
fn no_preview_offered(plugin: &str) -> String {
    format!(
        "{plugin} offered no preview of this call; allow without one by setting \
         `require_preview = false` under `[[plugin]] name = \"{plugin}\"`"
    )
}

/// What an answer to Allow is told when no preview of the call as it would
/// run has been shown: an answer from anywhere that shows none (a CLI, the
/// queue's card, the API) is sent to the card.
fn no_preview_shown(plugin: &str) -> String {
    format!(
        "no preview of this call as it would run has been shown, so it cannot be allowed \
         here; open the card to allow it. {plugin} requires one: allow without it by setting \
         `require_preview = false` under `[[plugin]] name = \"{plugin}\"`"
    )
}

/// What a page is shown of a preview's token, and sends back with its
/// answer to say which preview it read: the token's SHA-256.
pub fn preview_hash(token: &str) -> String {
    crate::corpus::hash_body(token)
}

/// Only a failed mutation transport or server error has an unknown remote
/// outcome. Read-only preflight failures are safe to retry.
fn remote_outcome_unknown(error: &str) -> bool {
    error.starts_with("mutation-outcome-unknown:")
}
fn refusal(decision: Decision) -> String {
    format!(
        "refused by policy{}: {}",
        decision
            .rule_id
            .map(|r| format!(" ({r})"))
            .unwrap_or_default(),
        decision.reason.unwrap_or_default()
    )
}

/// `name` plus its arguments on one line, bounded, for the policy haystack
/// and the queue card. Secrets never appear here: arguments are the
/// harness's own words. A profile is spelled out as `profile=<name>` ahead of
/// the JSON, so a deny rule can name one. A call that carries prose reads as
/// its target and first line instead.
pub fn summarize(name: &str, args: &Value) -> String {
    let mut s = match (
        prose_title(name, args),
        args.get("profile").and_then(Value::as_str),
    ) {
        (Some(title), _) => title,
        (None, Some(p)) => format!("{name} profile={p} {args}"),
        (None, None) => format!("{name} {args}"),
    };
    bound(&mut s);
    s
}

/// What a held call would replace, as it stands now, for the approval page
/// to show the proposal against: fields merged into the approval's details.
/// `None` for a tool that replaces nothing the node holds.
pub fn approval_context(
    store: &crate::store::Store,
    channel: &str,
    tool: &str,
    args: &Value,
) -> Result<Option<Value>, crate::store::StoreError> {
    match tool {
        docs::DOC_WRITE => {
            let current = docs::replaced(store, channel, args)?;
            Ok(Some(docs::approval_context(current.as_ref(), args)))
        }
        docs::DOC_EDIT => {
            let current = docs::replaced(store, channel, args)?;
            Ok(Some(docs::edit_approval_context(current.as_ref(), args)))
        }
        memory::AMEND => Ok(Some(memory::approval_context(store, channel, args)?)),
        _ => Ok(None),
    }
}

/// The longest excerpt of a plugin call's statement or text its title
/// carries, in characters.
const PLUGIN_TITLE_EXCERPT: usize = 120;

/// What a plugin call is, as the node reads it from the call's arguments:
/// its tool and what it targets (` on <profile>`, or its `key`). Every title
/// of a plugin's card starts with the plugin's name and this, so the plugin's
/// own words never stand in for which tool runs where.
fn plugin_call(tool: &str, args: &Value) -> String {
    let text = |key: &str| args.get(key).and_then(Value::as_str).and_then(first_line);
    match (text("profile"), text("key")) {
        (Some(profile), _) => format!("{tool} on {profile}"),
        (None, Some(key)) => format!("{tool} {key}"),
        (None, None) => tool.to_string(),
    }
}

/// A plugin call's card title before its preview is shown: the plugin and
/// its call ([`plugin_call`]), and the first line of the statement or text
/// it carries (`sql`, `summary` or `body`), bounded. Never its other
/// arguments, so bind `params` stay off the card and out of the push.
pub fn plugin_title(plugin: &str, tool: &str, args: &Value) -> String {
    let mut title = format!("{plugin}: {}", plugin_call(tool, args));
    if let Some(line) = ["sql", "summary", "body"]
        .into_iter()
        .find_map(|key| args.get(key).and_then(Value::as_str).and_then(first_line))
    {
        title.push_str(" · ");
        title.push_str(&excerpt(line, PLUGIN_TITLE_EXCERPT));
    }
    bound(&mut title);
    title
}

/// A plugin call's card title from its shown preview: the plugin and its
/// call ([`plugin_call`]), then the first line of the preview's `summary`
/// in place of the call's own text. A summary that names its plugin first
/// is not prefixed twice, and one that already opens with the call
/// (jira-review's `update WRK-12 on <site>`) stands for it. `None` for a
/// blank summary, which leaves the title it had.
pub fn preview_title(plugin: &str, tool: &str, args: &Value, summary: &str) -> Option<String> {
    let line = first_line(summary)?;
    let line = line
        .strip_prefix(plugin)
        .and_then(|rest| rest.strip_prefix(':').or_else(|| rest.strip_prefix(' ')))
        .and_then(first_line)
        .unwrap_or(line);
    let call = plugin_call(tool, args);
    let opens_with_call = line
        .strip_prefix(call.as_str())
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
    let mut title = match opens_with_call {
        true => format!("{plugin}: {line}"),
        false => format!("{plugin}: {call} · {line}"),
    };
    bound(&mut title);
    Some(title)
}

/// The first line of `text` that is not blank, trimmed.
fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| !l.is_empty())
}

/// `text` cut to `max` characters, marked when it was cut.
fn excerpt(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// A title cut to the length a card and a push notification carry.
fn bound(s: &mut String) {
    if s.len() > 400 {
        let mut cut = 400;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
}

/// A title a person reads for a call that carries prose: what it acts on and
/// the first line of what it says, rather than its arguments as JSON.
fn prose_title(name: &str, args: &Value) -> Option<String> {
    let text = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
    };
    let id = |key: &str| match args.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => "?".into(),
    };
    let first_line =
        |key: &str| text(key).and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()));
    let with = |head: String, said: Option<&str>| match said {
        Some(said) => format!("{head}: {said}"),
        None => head,
    };
    let short = |key: &str| id(key).chars().take(7).collect::<String>();
    // Variable names, not values: which ones are set is what an operator
    // weighs, and the page shows the values.
    let with_variables = |head: String| {
        let names: Vec<&str> = args
            .get("variables")
            .and_then(Value::as_object)
            .map(|v| v.keys().map(String::as_str).collect())
            .unwrap_or_default();
        if names.is_empty() {
            head
        } else {
            format!("{head} with {}", names.join(", "))
        }
    };
    Some(match name {
        github::PR_COMMENT | github::ISSUE_COMMENT | github::PR_REPLY => with(
            format!("{name} {}#{}", id("repo"), id("number")),
            first_line("body"),
        ),
        gitlab::MR_COMMENT | gitlab::MR_REPLY => with(
            format!("{name} {}!{}", id("project"), id("iid")),
            first_line("body"),
        ),
        docs::DOC_WRITE | docs::DOC_EDIT | docs::DOC_DELETE => format!("{name} {}", id("slug")),
        memory::RETAIN => with(format!("{name} {}", id("kind")), first_line("body")),
        // A memory has no name; the page shows which one beside the change.
        memory::AMEND => with(name.to_string(), first_line("reason")),
        work::WORK_UPDATE => {
            let fields: Vec<String> = [
                "title", "body", "priority", "deps", "project", "worktree", "state",
            ]
            .into_iter()
            .filter(|f| args.get(*f).is_some_and(|v| !v.is_null()))
            .map(|f| match (f, text(f)) {
                ("state" | "priority", Some(v)) => format!("{f} {v}"),
                _ => f.to_string(),
            })
            .collect();
            let head = format!(
                "{name} {} ({})",
                crate::names::handle(crate::names::kind::TASK, &id("id")),
                fields.join(", ")
            );
            with(head, first_line("title"))
        }
        work::WORK_DELETE | work::WORK_SHOW => match text("id") {
            Some(item) => format!(
                "{name} {}",
                crate::names::handle(crate::names::kind::TASK, item)
            ),
            None => name.to_string(),
        },
        work::WORK_LIST => {
            let narrowed: Vec<String> = ["project", "worktree", "state", "query"]
                .into_iter()
                .filter_map(|f| text(f).map(|v| format!("{f} {v}")))
                .collect();
            if narrowed.is_empty() {
                name.to_string()
            } else {
                format!("{name} ({})", narrowed.join(", "))
            }
        }
        review::SUBMIT_REPORT => with(name.to_string(), text("title")),
        // The whole sha: two heads that share a prefix read alike (#1049).
        github::PR_MERGE => format!(
            "{name} {}#{}: {} at {}",
            id("repo"),
            id("number"),
            text("method").unwrap_or("squash"),
            id("head_sha")
        ),
        gitlab::MR_MERGE => format!(
            "{name} {}!{}: {} at {}",
            id("project"),
            id("iid"),
            if args.get("squash") == Some(&Value::Bool(false)) {
                "merge"
            } else {
                "squash"
            },
            short("head_sha")
        ),
        github::PR_READY => format!(
            "{name} {}#{}: {}",
            id("repo"),
            id("number"),
            ready_or_draft(args)
        ),
        gitlab::MR_READY => format!(
            "{name} {}!{}: {}",
            id("project"),
            id("iid"),
            ready_or_draft(args)
        ),
        gitlab::JOB_PLAY => {
            with_variables(format!("{name} {}: job {}", id("project"), id("job_id")))
        }
        gitlab::PIPELINE_RUN => {
            let head = with_variables(format!("{name} {}: {}", id("project"), id("ref")));
            // Unlike a variable's, an input's value is short and typed, and
            // it is what decides which jobs the pipeline has, so it is named.
            let inputs: Vec<String> = args
                .get("inputs")
                .and_then(Value::as_object)
                .map(|v| v.iter().map(|(k, v)| format!("{k}={v}")).collect())
                .unwrap_or_default();
            if inputs.is_empty() {
                head
            } else {
                format!("{head}; inputs {}", inputs.join(", "))
            }
        }
        gitlab::PIPELINE_LIST_BY_SHA => format!("{name} {}: {}", id("project"), short("sha")),
        gitlab::DEPLOY => format!(
            "{name} {}: {} from job {} of pipeline {} at {}",
            id("project"),
            id("environment"),
            id("job_id"),
            id("pipeline_id"),
            short("source_sha")
        ),
        github::RUN_RERUN => format!("{name} {}: failed jobs of run {}", id("repo"), id("run_id")),
        gitlab::PIPELINE_RETRY => format!(
            "{name} {}: failed jobs of pipeline {}",
            id("project"),
            id("pipeline_id")
        ),
        github::RUN_DISPATCH => {
            let inputs: Vec<&str> = args
                .get("inputs")
                .and_then(Value::as_object)
                .map(|v| v.keys().map(String::as_str).collect())
                .unwrap_or_default();
            let head = format!("{name} {}: {} on {}", id("repo"), id("workflow"), id("ref"));
            if inputs.is_empty() {
                head
            } else {
                format!("{head} with {}", inputs.join(", "))
            }
        }
        services::START => format!("{name} {}", id("name")),
        setup::PROPOSE => with(format!("{name} {}", id("repo")), first_line("why")),
        setup::TRY => format!(
            "{name} {}: build {} from {}",
            id("repo"),
            id("dockerfile"),
            id("build_source")
        ),
        _ => return None,
    })
}

/// What a `pr_ready` or `mr_ready` call asks for, in words: ready unless it
/// says `ready: false`.
fn ready_or_draft(args: &Value) -> &'static str {
    if args.get("ready") == Some(&Value::Bool(false)) {
        "back to draft"
    } else {
        "ready for review"
    }
}

fn tool_result(value: &Value, is_error: bool) -> Value {
    let text = match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// The last `max` bytes of a log, cut on a character boundary.
pub(super) fn tail(log: &str, max: usize) -> (&str, bool) {
    if log.len() <= max {
        return (log, false);
    }
    let mut start = log.len() - max;
    while !log.is_char_boundary(start) {
        start += 1;
    }
    (&log[start..], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prose_call_is_titled_by_its_target_and_first_line() {
        let comment = json!({ "repo": "o/n", "number": 4, "body": "\n  Looks good.\nMore." });
        assert_eq!(
            summarize(github::PR_COMMENT, &comment),
            "pr_comment o/n#4: Looks good."
        );
        assert_eq!(
            summarize(github::ISSUE_COMMENT, &comment),
            "github_issue_comment o/n#4: Looks good."
        );
        let note = json!({ "project": "g/p", "iid": 9, "body": "Thanks" });
        assert_eq!(
            summarize(gitlab::MR_COMMENT, &note),
            "mr_comment g/p!9: Thanks"
        );
        let fact = json!({ "kind": "fact", "body": "The importer retries twice.\nThen gives up." });
        assert_eq!(
            summarize(memory::RETAIN, &fact),
            "retain fact: The importer retries twice."
        );
        let doc = json!({ "slug": "plan-foo", "body": "# Plan" });
        assert_eq!(summarize(docs::DOC_WRITE, &doc), "doc_write plan-foo");
        let id = "0b0e29c44da42ee5fef94df7b1b9eff70b0e29c44da42ee5fef94df7b1b9eff7";
        let task = crate::names::task(id);
        assert_eq!(
            summarize(
                work::WORK_UPDATE,
                &json!({ "id": id, "priority": "Urgent", "body": "x", "title": "Lists past 100" })
            ),
            format!("work_update {task} (title, body, priority Urgent): Lists past 100")
        );
        assert_eq!(
            summarize(work::WORK_DELETE, &json!({ "id": id })),
            format!("work_delete {task}")
        );
        // A prefix the agent shortened an id to names nothing: it reads as given.
        assert_eq!(
            summarize(work::WORK_SHOW, &json!({ "id": "0b0e29c4" })),
            "work_show 0b0e29c4"
        );
        assert_eq!(
            summarize(memory::AMEND, &json!({ "id": id, "reason": "Out of date" })),
            "memory_amend: Out of date"
        );
        assert_eq!(
            summarize(
                work::WORK_LIST,
                &json!({ "project": "tracon", "state": "all" })
            ),
            "work_list (project tracon, state all)"
        );
        assert_eq!(summarize(work::WORK_SHOW, &json!({})), "work_show");
        let query = json!({ "profile": "qa", "sql": "select 1" });
        assert!(
            summarize("sql-review__query", &query).starts_with("sql-review__query profile=qa {")
        );
    }

    /// A plugin call's card reads as the plugin, its tool, its target and
    /// the first line of its statement: never its arguments as JSON, and
    /// never its bind values.
    #[test]
    fn a_plugin_call_is_titled_without_its_arguments_as_json() {
        let write = json!({
            "operation_id": "afa-tst-plugin-write-a",
            "params": { "v": "PlugTestA" },
            "profile": "tst",
            "sql": "UPDATE app4adm.adm_app_data SET first_name = :v\nWHERE app_id = 252111",
        });
        let title = plugin_title("sql-review", "write", &write);
        assert_eq!(
            title,
            "sql-review: write on tst · UPDATE app4adm.adm_app_data SET first_name = :v"
        );
        assert!(
            !title.contains('{') && !title.contains("PlugTestA"),
            "{title}"
        );
        assert_eq!(
            plugin_title(
                "jira-review",
                "update",
                &json!({ "key": "WRK-12", "fields": { "summary": "secret" } })
            ),
            "jira-review: update WRK-12"
        );
        assert_eq!(
            plugin_title(
                "jira-review",
                "comment",
                &json!({ "key": "WRK-1", "body": "\n Done.\nMore" })
            ),
            "jira-review: comment WRK-1 · Done."
        );
        assert_eq!(
            plugin_title("fake", "describe", &json!({})),
            "fake: describe"
        );
        let long = plugin_title("sql-review", "query", &json!({ "sql": "x".repeat(500) }));
        assert_eq!(
            long,
            format!("sql-review: query · {}…", "x".repeat(PLUGIN_TITLE_EXCERPT))
        );
    }

    /// A shown preview's one line titles the card, behind the plugin and
    /// the call as the node read it: a summary can add to the title but
    /// not change which tool runs where.
    #[test]
    fn a_preview_summary_titles_a_plugin_call() {
        let write = json!({ "profile": "tst", "params": { "v": "PlugTestA" }, "sql": "UPDATE t" });
        // sql-review's summary is the statement and its rows.
        assert_eq!(
            preview_title(
                "sql-review",
                "write",
                &write,
                "UPDATE t SET a = :v (1 row(s))\nmore"
            )
            .as_deref(),
            Some("sql-review: write on tst · UPDATE t SET a = :v (1 row(s))")
        );
        // jira-review's opens with the call, so it is not said twice.
        let update = json!({ "key": "WRK-12", "summary": "New title" });
        assert_eq!(
            preview_title(
                "jira-review",
                "update",
                &update,
                "update WRK-12 on https://example.atlassian.net"
            )
            .as_deref(),
            Some("jira-review: update WRK-12 on https://example.atlassian.net")
        );
        // A summary that names another target leaves the call's own in front.
        assert_eq!(
            preview_title("jira-review", "update", &update, "update WRK-120 on site").as_deref(),
            Some("jira-review: update WRK-12 · update WRK-120 on site")
        );
        // Nor can it pose as another tool or profile.
        assert_eq!(
            preview_title(
                "sql-review",
                "write",
                &write,
                "sql-review: query on dev (0 rows)"
            )
            .as_deref(),
            Some("sql-review: write on tst · query on dev (0 rows)")
        );
        assert_eq!(
            preview_title("sql-review", "write", &write, "jira-review: update WRK-1").as_deref(),
            Some("sql-review: write on tst · jira-review: update WRK-1")
        );
        assert_eq!(preview_title("fake", "describe", &json!({}), " \n "), None);
        let long = preview_title("fake", "describe", &json!({}), &"y".repeat(1000)).unwrap();
        assert!(long.len() <= 404 && long.ends_with('…'), "{long}");
    }

    /// A gated call with no prose is titled in words too, on the card and in
    /// the push, rather than as its arguments.
    #[test]
    fn a_gated_call_is_titled_by_what_it_does() {
        let merge = json!({ "repo": "o/n", "number": 412, "head_sha": "3e1f0a2c9b8d4e5f60718293a4b5c6d7e8f90a1b", "method": "squash", "operation_id": "op" });
        assert_eq!(
            summarize(github::PR_MERGE, &merge),
            "pr_merge o/n#412: squash at 3e1f0a2c9b8d4e5f60718293a4b5c6d7e8f90a1b"
        );
        let mr = json!({ "project": "g/p", "iid": 9, "head_sha": "abcdef12", "squash": false, "operation_id": "op" });
        assert_eq!(
            summarize(gitlab::MR_MERGE, &mr),
            "mr_merge g/p!9: merge at abcdef1"
        );
        let ready = json!({ "repo": "o/n", "number": 595 });
        assert_eq!(
            summarize(github::PR_READY, &ready),
            "pr_ready o/n#595: ready for review"
        );
        let draft = json!({ "project": "g/p", "iid": 9, "ready": false });
        assert_eq!(
            summarize(gitlab::MR_READY, &draft),
            "mr_ready g/p!9: back to draft"
        );
        let rerun = json!({ "repo": "o/n", "run_id": 11893472205_u64 });
        assert_eq!(
            summarize(github::RUN_RERUN, &rerun),
            "run_rerun o/n: failed jobs of run 11893472205"
        );
        let retry = json!({ "project": "g/p", "pipeline_id": 12 });
        assert_eq!(
            summarize(gitlab::PIPELINE_RETRY, &retry),
            "pipeline_retry g/p: failed jobs of pipeline 12"
        );
        let dispatch = json!({ "repo": "o/n", "workflow": "renovate.yaml", "ref": "main",
                               "inputs": { "LOG_LEVEL": "debug" } });
        assert_eq!(
            summarize(github::RUN_DISPATCH, &dispatch),
            "run_dispatch o/n: renovate.yaml on main with LOG_LEVEL"
        );
        let service = json!({ "name": "postgres", "wait_secs": 30 });
        assert_eq!(
            summarize(services::START, &service),
            "service_start postgres"
        );
        let propose = json!({ "repo": "/src/orbit", "checks": ["cargo test"], "why": "\nTried it twice.\nBoth passed." });
        assert_eq!(
            summarize(setup::PROPOSE, &propose),
            "repo_setup_propose /src/orbit: Tried it twice."
        );
        let play = json!({ "project": "g/p", "job_id": 5, "variables": { "TARGET": "qa" } });
        assert_eq!(
            summarize(gitlab::JOB_PLAY, &play),
            "job_play g/p: job 5 with TARGET"
        );
        let run = json!({ "project": "g/p", "ref": "main" });
        assert_eq!(
            summarize(gitlab::PIPELINE_RUN, &run),
            "pipeline_run g/p: main"
        );
        let run = json!({ "project": "g/p", "ref": "feat/x", "variables": { "TARGET": "qa" },
                          "inputs": { "build-dev-images": true, "environment": "staging" } });
        assert_eq!(
            summarize(gitlab::PIPELINE_RUN, &run),
            "pipeline_run g/p: feat/x with TARGET; inputs build-dev-images=true, environment=\"staging\""
        );
        let deploy = json!({ "project": "g/p", "pipeline_id": 12, "job_id": 7, "environment": "qa", "source_sha": "abcdef12", "operation_id": "op" });
        assert_eq!(
            summarize(gitlab::DEPLOY, &deploy),
            "deploy g/p: qa from job 7 of pipeline 12 at abcdef1"
        );
    }

    /// Every tool the operator can be asked about has a title in words, so a
    /// new gated tool cannot reach the card or a push as raw JSON unnoticed.
    /// Asked means: the shipped bundle asks with no arguments, or the node
    /// always holds it.
    #[test]
    fn every_asked_tool_is_titled_in_words() {
        let policy = crate::policy::Policy::shipped();
        let none = json!({});
        let mut asked: Vec<&str> = schema::tool_names()
            .filter(|name| {
                let decision = policy.decide(&Request {
                    channel: "work",
                    action: name,
                    kind: Some(TOOL_KIND),
                    resource: None,
                    command: None,
                    arguments: Some(&none),
                    doc_kinds: None,
                });
                decision.verdict == Verdict::Ask
            })
            // Routed before the policy: these hold their own approval, with
            // their own title (egress), or none at all.
            .filter(|name| {
                ![
                    operator::ASK,
                    operator::NOTIFY,
                    operator::REPORT,
                    operator::REPORT_STATUS,
                    operator::QUESTION_STATUS,
                    egress::REQUEST,
                    setup::DRAFT,
                    setup::TRY,
                ]
                .contains(name)
            })
            .chain([setup::PROPOSE])
            .collect();
        asked.sort();
        asked.dedup();
        assert!(asked.contains(&github::PR_MERGE), "{asked:?}");
        let raw: Vec<String> = asked
            .iter()
            .map(|name| summarize(name, &none))
            .filter(|title| title.contains('{'))
            .collect();
        assert!(raw.is_empty(), "titled as JSON: {raw:?}");
    }

    /// A refused `operation_id` is named, with its pattern, and not blamed on
    /// the other arguments; a dot is refused (it is not in the pattern) and
    /// dashes are accepted. Every consequential schema states the pattern.
    #[test]
    fn a_refused_operation_id_is_named_with_its_pattern() {
        let call = |id: &str| json!({ "repo": "o/n", "number": 11, "head_sha": "bb928d2", "method": "squash", "operation_id": id });
        assert!(consequential(github::PR_MERGE, &call("merge-hora-7-release-0-3-0")).is_some());
        let dotted = call("merge-hora-7-release-0.3.0");
        assert!(consequential(github::PR_MERGE, &dotted).is_none());
        let refusal = consequential_refusal(github::PR_MERGE, &dotted);
        assert!(refusal.contains("operation_id"), "{refusal}");
        assert!(refusal.contains(OPERATION_ID_PATTERN), "{refusal}");
        assert!(refusal.contains("merge-hora-7-release-0.3.0"), "{refusal}");
        assert!(!refusal.contains("canonical arguments"), "{refusal}");

        let short = consequential_refusal(github::PR_MERGE, &call("x-1-2"));
        assert!(short.contains(OPERATION_ID_PATTERN), "{short}");
        let missing = consequential_refusal(
            gitlab::DEPLOY,
            &json!({ "project": "g/p", "pipeline_id": 1, "job_id": 2, "environment": "qa" }),
        );
        assert!(missing.contains("requires an operation_id"), "{missing}");

        // A good id with a missing target blames the target, not the id.
        let unscoped = json!({ "number": 11, "operation_id": "merge-hora-7" });
        assert!(consequential(github::PR_MERGE, &unscoped).is_none());
        let refusal = consequential_refusal(github::PR_MERGE, &unscoped);
        assert!(!refusal.contains("operation_id"), "{refusal}");
        assert!(refusal.contains("repo"), "{refusal}");

        for tool in github::definitions()
            .iter()
            .chain(gitlab::definitions().iter())
        {
            let schema = &tool["inputSchema"]["properties"]["operation_id"];
            if schema.is_null() {
                continue;
            }
            assert_eq!(schema["pattern"], OPERATION_ID_PATTERN, "{}", tool["name"]);
            assert!(
                schema["description"].as_str().unwrap().contains("no dots"),
                "{}",
                tool["name"]
            );
        }
        let plugin = with_operation_id(&json!({ "type": "object" }));
        assert_eq!(
            plugin["properties"]["operation_id"]["pattern"],
            OPERATION_ID_PATTERN
        );
    }

    #[test]
    fn a_merge_default_is_written_into_the_arguments_that_are_approved() {
        let pr = json!({ "repo": "o/n", "number": 1, "head_sha": "abcdef1", "operation_id": "op" });
        let resolved = with_provider_defaults(github::PR_MERGE, &pr, None).unwrap();
        assert_eq!(resolved["method"], "squash");
        assert_eq!(
            canonical_consequential_payload(github::PR_MERGE, &resolved),
            canonical_consequential_payload(github::PR_MERGE, &pr),
        );
        let chosen = json!({ "method": "rebase", "repo": "o/n" });
        assert!(with_provider_defaults(github::PR_MERGE, &chosen, None).is_none());

        let mr = json!({ "project": "g/p", "iid": 2, "head_sha": "abc", "operation_id": "op" });
        assert_eq!(
            with_provider_defaults(gitlab::MR_MERGE, &mr, None).unwrap()["squash"],
            true
        );
        let kept = json!({ "squash": false });
        assert!(with_provider_defaults(gitlab::MR_MERGE, &kept, None).is_none());
        assert!(with_provider_defaults(github::PR_COMMENT, &pr, None).is_none());
    }

    /// The session's repository is written in before anything hashes the
    /// arguments: a merge retried without `repo` is the same request, under
    /// the same replay record, as the one that named it.
    #[test]
    fn the_session_repository_is_a_default_like_any_other() {
        let github = crate::forge::ForgeRepo::from_remote("github.com/o/n").unwrap();
        let gitlab = crate::forge::ForgeRepo::from_remote("gitlab.com/g/p").unwrap();

        let named =
            json!({ "repo": "o/n", "number": 1, "head_sha": "abcdef1", "operation_id": "op" });
        let unnamed = json!({ "number": 1, "head_sha": "abcdef1", "operation_id": "op" });
        let filled = with_provider_defaults(github::PR_MERGE, &unnamed, Some(&github)).unwrap();
        assert_eq!(filled["repo"], "o/n");
        assert_eq!(
            canonical_consequential_payload(github::PR_MERGE, &filled),
            canonical_consequential_payload(
                github::PR_MERGE,
                &with_provider_defaults(github::PR_MERGE, &named, Some(&github)).unwrap()
            ),
        );
        assert_eq!(
            request_key(github::PR_MERGE, &filled),
            request_key(
                github::PR_MERGE,
                &with_provider_defaults(github::PR_MERGE, &named, Some(&github)).unwrap()
            ),
        );
        // Without the default there is no repository to hash at all.
        assert!(canonical_consequential_payload(github::PR_MERGE, &unnamed).is_none());

        // An explicit repository wins; a blank one names nothing.
        let other = json!({ "repo": "else/where", "branch": "main" });
        assert!(with_provider_defaults(github::RUN_STATUS, &other, Some(&github)).is_none());
        let blank = json!({ "repo": " ", "branch": "main" });
        assert_eq!(
            with_provider_defaults(github::RUN_STATUS, &blank, Some(&github)).unwrap()["repo"],
            "o/n"
        );

        // GitLab's tools take it as `project`.
        let mr = json!({ "iid": 2, "head_sha": "abc", "operation_id": "op" });
        let filled = with_provider_defaults(gitlab::MR_MERGE, &mr, Some(&gitlab)).unwrap();
        assert_eq!(
            (&filled["project"], &filled["squash"]),
            (&json!("g/p"), &json!(true))
        );
        assert!(filled.get("repo").is_none());

        // Every forge tool takes the repository by the forge's own name for it.
        for (tools, names, key) in [
            (github::definitions(), github::TOOLS, "repo"),
            (gitlab::definitions(), gitlab::TOOLS, "project"),
        ] {
            assert_eq!(tools.len(), names.len());
            for tool in tools {
                let name = tool["name"].as_str().unwrap();
                assert!(forge_of_tool(name).is_some(), "{name} is not a forge tool");
                assert!(
                    tool["inputSchema"]["properties"].get(key).is_some(),
                    "{name} has no {key}"
                );
            }
        }
        assert_eq!(forge_of_tool(review::SUBMIT), None);
    }

    /// `submit_review` takes the session's repository as provider and
    /// project: both filled when both are missing, the other filled when the
    /// one given is the session's own, refused when it is not, and never
    /// touched when both are given.
    #[test]
    fn a_submission_defaults_to_the_session_repository_or_is_refused() {
        let github = crate::forge::ForgeRepo::from_remote("github.com/o/n").unwrap();
        let gitlab = crate::forge::ForgeRepo::from_remote("gitlab.com/g/sub/p").unwrap();
        let bare = json!({ "title": "t", "body": "b" });

        let filled = submission_defaults(&bare, Some(&github)).unwrap().unwrap();
        assert_eq!(
            (&filled["provider"], &filled["project"]),
            (&json!("github"), &json!("o/n"))
        );
        let named = json!({ "title": "t", "body": "b", "provider": "github", "project": "o/n" });
        assert_eq!(
            request_key(review::SUBMIT, &filled),
            request_key(review::SUBMIT, &named)
        );
        let filled = submission_defaults(&bare, Some(&gitlab)).unwrap().unwrap();
        assert_eq!(
            (&filled["provider"], &filled["project"]),
            (&json!("gitlab"), &json!("g/sub/p"))
        );

        // Both given: untouched, wherever they point.
        let elsewhere = json!({ "provider": "gitlab", "project": "x/y" });
        assert_eq!(submission_defaults(&elsewhere, Some(&github)), Ok(None));
        // One given and consistent: the other is filled. Blank is missing.
        for (given, filled_key, value) in [
            (json!({ "provider": "github" }), "project", "o/n"),
            (
                json!({ "project": "O/N", "provider": " " }),
                "provider",
                "github",
            ),
        ] {
            let out = submission_defaults(&given, Some(&github)).unwrap().unwrap();
            assert_eq!(out[filled_key], value, "{given}");
        }
        // One given and not the session's: refused, naming both sides.
        for given in [json!({ "provider": "gitlab" }), json!({ "project": "x/y" })] {
            let refused = submission_defaults(&given, Some(&github)).unwrap_err();
            assert!(refused.contains("github `o/n`"), "{refused}");
            assert!(refused.contains("give both"), "{refused}");
        }
        // No session repository: nothing filled, nothing refused here.
        assert_eq!(submission_defaults(&bare, None), Ok(None));
        assert_eq!(
            submission_defaults(&json!({ "project": "x/y" }), None),
            Ok(None)
        );
    }

    fn tools(store: &str) -> Tools {
        Tools {
            broker: toml::from_str::<crate::broker::Broker>(store)
                .unwrap()
                .shared(),
            cfg: Arc::new(Config::default()),
            policy: Policy::shipped_shared(),
            http: reqwest::Client::new(),
            session: Default::default(),
            upstream: Default::default(),
        }
    }

    fn ctx(channel: &str) -> CallContext {
        CallContext::session("s", channel, "n1")
    }

    const STORE: &str = r#"
        [credentials.gh]
        channels = ["work"]
        [credentials.gh.env]
        GH_TOKEN = "t"
    "#;

    #[tokio::test]
    async fn tools_are_offered_only_to_a_bound_channel() {
        let t = tools(STORE);
        assert_eq!(t.list("work", "n1").len(), github::definitions().len());
        assert!(t.list("personal", "n1").is_empty());
    }

    /// A profile is spelled out in the policy haystack, so a deny rule can
    /// name one: `sql-review`'s tools take one per database account.
    #[tokio::test]
    async fn a_deny_rule_can_name_a_profile() {
        let mut t = tools(STORE);
        t.policy = Arc::new(parking_lot::RwLock::new(
            toml::from_str(
                r#"
            version = 9
            [[rule]]
            id = "no-production"
            verdict = "deny"
            reason = "Production is read by hand."
            kinds = ["tool"]
            matches = ["profile=prd"]
            "#,
            )
            .unwrap(),
        ));
        let res = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
                        "params":{"name":"sql-review__query","arguments":{"sql":"SELECT 1","profile":"prd"}}}),
            )
            .await
            .unwrap();
        assert_eq!(res["result"]["isError"], true);
        let text = res["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("no-production"), "{text}");
    }

    #[tokio::test]
    async fn initialize_and_tools_list_answer() {
        let t = tools(STORE);
        let init = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
            )
            .await
            .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "tracon");
        let list = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            )
            .await
            .unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&github::PR_STATUS) && names.contains(&github::PR_THREADS));
    }

    /// The only client here is a harness this node launched. One announcing
    /// an MCP revision the node does not serve is a compatibility fault worth
    /// seeing: it is refused by name rather than answered with a version
    /// number it did not ask for and cannot read.
    #[tokio::test]
    async fn an_unsupported_mcp_revision_is_refused_by_name() {
        let t = tools(STORE);
        let res = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                        "params":{"protocolVersion":"2099-01-01"}}),
            )
            .await
            .unwrap();
        assert!(res["result"].is_null(), "{res}");
        let message = res["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("2099-01-01"), "{message}");
        assert!(message.contains(PROTOCOL_VERSION), "{message}");
    }

    /// A revision this server does serve is answered in that revision: the
    /// surface is identical across them, and telling a client a number it did
    /// not ask for is what makes it give up.
    #[tokio::test]
    async fn a_supported_mcp_revision_is_answered_in_its_own_terms() {
        let t = tools(STORE);
        for asked in SUPPORTED_PROTOCOL_VERSIONS {
            let res = t
                .handle(
                    &ctx("work"),
                    &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                            "params":{"protocolVersion":asked}}),
                )
                .await
                .unwrap();
            assert_eq!(res["result"]["protocolVersion"], asked, "{res}");
        }
    }

    #[tokio::test]
    async fn a_notification_gets_no_response() {
        let t = tools(STORE);
        assert!(t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .await
            .is_none());
    }

    #[tokio::test]
    async fn an_unbound_channel_cannot_call_the_tool_even_knowing_its_name() {
        // Not offering the tool is presentation; refusing the call is the gate.
        let t = tools(STORE);
        let res = t
            .handle(
                &ctx("personal"),
                &json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                        "params":{"name":"pr_status","arguments":{"repo":"o/n","number":1}}}),
            )
            .await
            .unwrap();
        assert_eq!(res["result"]["isError"], true);
        let text = res["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not bound"), "{text}");
    }

    #[tokio::test]
    async fn a_tool_the_policy_denies_is_refused_with_the_reason() {
        let mut t = tools(STORE);
        t.policy = Arc::new(parking_lot::RwLock::new(
            toml::from_str(
                r#"
            version = 9
            [[rule]]
            id = "no-warehouse"
            verdict = "deny"
            reason = "The warehouse is closed today."
            kinds = ["tool"]
            matches = ["pr_status"]
            "#,
            )
            .unwrap(),
        ));
        let res = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":5,"method":"tools/call",
                        "params":{"name":"pr_status","arguments":{"repo":"o/n","number":1}}}),
            )
            .await
            .unwrap();
        assert_eq!(res["result"]["isError"], true);
        let text = res["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("no-warehouse") && text.contains("closed today"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_tool_the_policy_does_not_cover_is_not_run_unattended() {
        // Empty policy: everything is asked, and with no session to ask
        // through the call fails rather than proceeds.
        let mut t = tools(STORE);
        t.policy = Default::default();
        let res = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":6,"method":"tools/call",
                        "params":{"name":"pr_status","arguments":{"repo":"o/n","number":1}}}),
            )
            .await
            .unwrap();
        assert_eq!(res["result"]["isError"], true);
        let text = res["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("approval"), "{text}");
    }
}
