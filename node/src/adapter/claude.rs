//! The Claude Code adapter. Drives `claude` headless over stream-json on the
//! runner's stdio.
//!
//! Its `control_request` / `control_response` path is what this adapter is
//! built on: the same protocol the published Agent SDKs speak — they spawn
//! this same binary and talk stream-json to it — so there is no reason to
//! take a Node dependency to reach it from Rust.
//!
//! The shapes below were read out of the shipped 2.1.247 binary and confirmed
//! against a live run, because the CLI's `--help` documents neither the
//! control protocol nor `--permission-prompt-tool`; 2.1.295 was checked
//! against them the same way.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::{
    AdapterError, HarnessAdapter, HarnessCompat, HarnessEvent, HarnessHandle, HarnessVersion,
    LaunchSpec, Layout, ModelOption, PermissionReply, PermissionRequest, ProtocolSupport,
    TurnResult,
};
use crate::adapter::types::{self, PermissionOption, ToolCall, ToolCallUpdate, Usage};
use crate::runner::{Runner, RunnerCommand, RunnerError, Spawned};

/// The models this adapter offers. Claude Code has no catalogue endpoint of
/// its own; these are the aliases it resolves to whatever is current, which is
/// also what keeps a pinned node from silently following a model change.
const ALIASES: &[&str] = &["opus", "sonnet", "haiku"];

pub struct ClaudeAdapter {
    pinned: String,
}

impl ClaudeAdapter {
    pub const ID: &'static str = "claude";

    /// Where the harness image installs the binary, and where a repository's
    /// image layered with it (`repo_image`) has it too. Named by path, so a
    /// repository image whose `PATH` holds another `claude` cannot stand in
    /// for the one the node pinned.
    pub const BINARY: &'static str = "/usr/local/bin/claude";

    /// The version this node's harness image installs.
    /// `containers/harness-claude/Containerfile` fetches exactly this release
    /// at exactly one digest; `the_image_installs_the_pinned_claude` keeps the
    /// two from drifting apart.
    pub const PINNED_VERSION: &'static str = "2.1.295";

    /// Claude Code's contract is the stream-json control protocol, which
    /// carries no version field of its own. Every shape this
    /// adapter drives was read from one revision of it, and 1 is the name
    /// given to that revision here. A `system/init` frame that grows a version
    /// and names something outside this range is refused; an init frame with
    /// no version at all is the revision these shapes came from, so absence is
    /// 1 rather than a refusal — unlike the harness version, which the CLI
    /// always reports and which therefore fails closed when it is missing.
    pub const PROTOCOL: ProtocolSupport = ProtocolSupport {
        name: "claude-stream-json",
        min: 1,
        max: 1,
    };

    pub fn new(pinned: impl Into<String>) -> Self {
        Self {
            pinned: pinned.into(),
        }
    }

    pub const fn layout() -> Layout {
        Layout {
            dir: ".claude",
            env: "CLAUDE_CONFIG_DIR",
        }
    }

    /// The launch manifest's skills, as a plugin the CLI loads for this
    /// session only, relative to the state directory and sealed read-only.
    /// `--setting-sources project` keeps the CLI from reading skills out of
    /// its own config directory, so a plugin named on the command line is the
    /// one way in that does not go through the worktree. Checked against
    /// 2.1.296: the `system/init` frame lists them as `tracon:<name>`.
    const PLUGIN_DIR: &'static str = "plugin";

    /// The node's MCP server as `--mcp-config` reads it, relative to the
    /// state directory and mounted read-only.
    const MCP_CONFIG_FILE: &'static str = "tracon-mcp.json";

    /// The built-in tools a node session is offered when the operator names
    /// none. `--allowedTools` only pre-approves, so without `--tools` every
    /// request carried all two dozen built-in definitions — about 27 K tokens,
    /// over half of a first request — for tools a headless, contained session
    /// never calls: planning modes, worktrees, cron, subagents, and web tools
    /// with no route out. `Skill` stays so the manifest's skills load.
    /// `AskUserQuestion` is left out on purpose: nobody can answer it
    /// headless, and `ask_operator` is the route to the operator.
    /// `ToolSearch` is [`Self::TOOL_SEARCH`].
    const DEFAULT_TOOLS: &'static [&'static str] = &[
        "Bash",
        "Read",
        "Edit",
        "Write",
        "TaskStop",
        "Skill",
        "ToolSearch",
    ];

    /// The built-in that loads deferred tools. With it named in `--tools` and
    /// [`TOOL_SEARCH_ENV`] set, the node's MCP tools reach the model by name
    /// only, and a definition is sent once the model asks for it: about 57 K
    /// characters of definitions a request become 10 K for an execute
    /// session (measured against 2.1.295). Either half alone loads them all
    /// up front. It is added to an operator's own list too: it decides how
    /// the node's tools are loaded, not what the session may do.
    const TOOL_SEARCH: &'static str = "ToolSearch";

    /// The built-ins for `--tools`: the operator's own list when it names
    /// any, by tool name (`Bash(git:*)` is `Bash`; the node's MCP tools are
    /// not built-ins), and [`Self::DEFAULT_TOOLS`] otherwise; always with
    /// [`Self::TOOL_SEARCH`].
    fn builtin_tools(configured: &[String]) -> Vec<String> {
        let mut named: Vec<String> = Vec::new();
        for tool in configured {
            let name = tool.split('(').next().unwrap_or(tool).trim();
            if name.is_empty() || name.starts_with("mcp__") || named.iter().any(|n| n == name) {
                continue;
            }
            named.push(name.to_string());
        }
        if named.is_empty() {
            return Self::DEFAULT_TOOLS.iter().map(|t| t.to_string()).collect();
        }
        if !named.iter().any(|n| n == Self::TOOL_SEARCH) {
            named.push(Self::TOOL_SEARCH.into());
        }
        named
    }

    /// The argv for a headless session.
    ///
    /// `--permission-mode default` is load-bearing and must never become
    /// `dontAsk` or `bypassPermissions`: those decide tool use inside the
    /// harness, and the node would never see the request it exists to broker.
    /// `--strict-mcp-config` is the other half — only the node's MCP server is
    /// reachable, whatever the container happens to contain.
    fn cmd(name: &str, spec: &LaunchSpec, session_id: &str) -> RunnerCommand {
        let mut argv: Vec<String> = vec![
            Self::BINARY.into(),
            "--print".into(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            "default".into(),
            // Where an ask goes. Without it the headless CLI does not send a
            // `can_use_tool` request at all: it denies anything not already
            // allowed ("you haven't granted it yet") and the node never sees
            // the ask. `stdio` routes it over the control protocol this
            // adapter answers, as the Agent SDKs do.
            "--permission-prompt-tool".into(),
            "stdio".into(),
            "--strict-mcp-config".into(),
            // Never read the operator's own settings or memory: the boundary
            // gives it a fresh home, and this makes that explicit rather than
            // incidental.
            "--setting-sources".into(),
            "project".into(),
            // A reopened session names the conversation it carries on, which
            // the CLI keeps its id for; `--session-id` alongside it is refused
            // ("can only be used with --continue or --resume if
            // --fork-session is also specified", seen from 2.1.311).
            if spec.resume.is_some() {
                "--resume".into()
            } else {
                "--session-id".into()
            },
            session_id.into(),
            "--add-dir".into(),
            spec.cwd_in_runner.clone(),
            "--model".into(),
            spec.model.clone(),
        ];
        // The servers carry this session's token as a header, so they are
        // read from a file staged beside the plugin (`scratch_files`) rather
        // than given on the argv, which a pod spec and `ps` both show.
        if !spec.mcp_servers.is_empty() {
            argv.push("--mcp-config".into());
            argv.push(format!(
                "{}/{}",
                crate::session::materialize::state_target(&spec.harness_home, Self::layout()),
                Self::MCP_CONFIG_FILE
            ));
        }
        argv.push("--plugin-dir".into());
        argv.push(format!(
            "{}/{}",
            crate::session::materialize::state_target(&spec.harness_home, Self::layout()),
            Self::PLUGIN_DIR
        ));
        if let Some(file) = &spec.system_prompt_file {
            argv.push("--append-system-prompt-file".into());
            argv.push(file.clone());
        }
        argv.push("--tools".into());
        argv.push(Self::builtin_tools(&spec.tools).join(","));
        if !spec.tools.is_empty() {
            argv.push("--allowedTools".into());
            argv.push(spec.tools.join(","));
        }
        // The binary is Bun, and `BUN_OPTIONS` is read by it: a `--preload`
        // there runs a script inside the harness before it starts (observed
        // against the pinned release). A repository image that sets one is
        // refused at its layer; the launch states it empty as well, so no
        // other value reaches the harness from anywhere.
        let mut env = spec.env.clone();
        env.push(("BUN_OPTIONS".into(), String::new()));
        env.push((TOOL_SEARCH_ENV.into(), "true".into()));
        RunnerCommand {
            argv,
            env,
            secret_env: spec.secret_env.clone(),
            workdir: Some(spec.cwd_in_runner.clone()),
            name: name.into(),
            ..Default::default()
        }
    }
}

/// Tool search, on. Claude Code turns it off by default when
/// `ANTHROPIC_BASE_URL` is not an Anthropic host, as the gateway never is,
/// until 2.1.304; the gateway forwards the `defer_loading` fields,
/// `tool_reference` blocks and `anthropic-beta` flag it needs. `true` and not
/// `auto`: auto counts the definitions against a share of the context window,
/// which the node's tools stay under while still costing every request.
/// 2.1.304 itself breaks every request behind a gateway when `ToolSearch` is
/// missing from `--tools` (fixed in 2.1.305), which [`ClaudeAdapter::TOOL_SEARCH`]
/// being always named also avoids.
const TOOL_SEARCH_ENV: &str = "ENABLE_TOOL_SEARCH";

/// The model as Claude Code names it. The node's picker spells models
/// `provider/id` (that is how tracon-agent takes them), and Claude Code passes its
/// `--model` to Anthropic verbatim, which answers 404 for `anthropic/…`. A
/// model another provider serves is refused rather than sent where it cannot
/// run.
fn claude_model(model: &str) -> Result<&str, AdapterError> {
    match model.split_once('/') {
        Some(("anthropic", id)) => Ok(id),
        Some((provider, _)) => Err(AdapterError::Protocol(format!(
            "the Claude Code harness runs Anthropic models only; {model} is served by {provider}"
        ))),
        None => Ok(model),
    }
}

/// The per-server `timeout` Claude Code applies to a call to the node's MCP
/// server. Left unset, its HTTP transport aborts a POST that has not answered
/// in 60 s (or `MCP_TIMEOUT`, if larger), and the node answers a tool call
/// with one response when the call ends. This value lifts that timer, the
/// tool-call wall clock and the 300 s idle watchdog to twenty minutes.
///
/// Under 2.1.247 live calls still ended at 5–5½ minutes with "The operation
/// timed out.", whatever this value said. 2.1.274 fixed "Streamable HTTP MCP
/// tool calls timing out after about 5 minutes even when a longer per-server
/// `timeout` was set" by passing Bun's `fetch` `timeout: false` once this
/// value exceeds 300 s, and the pinned release does that. A call held for
/// 330 s outside a session returned under 2.1.247 too, so which limit cut the
/// live calls is not pinned down.
///
/// Every tool that waits on a person still returns within
/// `mcp::wait::MAX_WAIT_SECS` with a handle its status tool waits on, since
/// twenty minutes is no bound on a person either. This value is headroom for
/// slow tools that are not waiting on anyone, not permission to block.
const MCP_CALL_TIMEOUT_MS: u64 = 20 * 60 * 1000;

/// The node builds one neutral MCP descriptor; Claude Code wants a map keyed
/// by server name, with headers as an object rather than a list.
fn mcp_config(servers: &[Value]) -> Value {
    let mut map = serde_json::Map::new();
    for s in servers {
        let name = s["name"].as_str().unwrap_or("tracon").to_string();
        let mut headers = serde_json::Map::new();
        if let Some(list) = s["headers"].as_array() {
            for h in list {
                if let (Some(n), Some(v)) = (h["name"].as_str(), h["value"].as_str()) {
                    headers.insert(n.to_string(), json!(v));
                }
            }
        }
        map.insert(
            name,
            json!({
                "type": s["type"].as_str().unwrap_or("http"),
                "url": s["url"],
                "headers": Value::Object(headers),
                "timeout": MCP_CALL_TIMEOUT_MS,
            }),
        );
    }
    json!({ "mcpServers": Value::Object(map) })
}

/// The stream-json revision a `system/init` frame names, defaulting to the one
/// these shapes were read from when it names none. Both spellings are accepted
/// because the frame is camelCase in places and snake_case in others.
fn init_protocol(init: &Value) -> u32 {
    init.get("protocol_version")
        .or_else(|| init.get("protocolVersion"))
        .and_then(Value::as_u64)
        .map(|v| v.try_into().unwrap_or(u32::MAX))
        .unwrap_or(ClaudeAdapter::PROTOCOL.min)
}

fn parse_version(out: &str) -> String {
    // "2.1.295 (Claude Code)"
    out.split_whitespace()
        .next()
        .filter(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or("unknown")
        .to_string()
}

/// The two options tracon's queue is built around. Claude Code's control
/// protocol is a straight allow/deny, so the adapter presents exactly those
/// and nothing that would not survive the round trip.
fn options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            option_id: types::OPTION_ALLOW_ONCE.into(),
            name: "Allow".into(),
            kind: "allow_once".into(),
        },
        PermissionOption {
            option_id: types::OPTION_REJECT_ONCE.into(),
            name: "Reject".into(),
            kind: "reject_once".into(),
        },
    ]
}

/// Writer half: every line the node sends the harness goes through here, so a
/// prompt and a permission answer cannot interleave mid-line.
#[derive(Clone)]
struct Writer(mpsc::Sender<String>);

impl Writer {
    fn spawn(mut stdin: Box<dyn AsyncWrite + Send + Unpin>) -> Self {
        let (tx, mut rx) = mpsc::channel::<String>(64);
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if stdin.write_all(b"\n").await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            // Dropping stdin is how a headless run is told there is no more
            // input; the process then exits on its own.
        });
        Self(tx)
    }

    async fn send(&self, v: Value) -> Result<(), AdapterError> {
        self.0
            .send(v.to_string())
            .await
            .map_err(|_| AdapterError::Protocol("the harness is gone".into()))
    }
}

/// The CLI's built-in command that compacts the conversation. Sent as a user
/// message it runs the CLI's own compaction rather than reaching the model as
/// text: a `compact_boundary` frame, the summary as a synthetic user message,
/// and a `result` of no turns (seen from 2.1.311 in stream-json mode).
const COMPACT_COMMAND: &str = "/compact";

/// What the CLI says, in the `result` that ends the first turn, when
/// `--resume` names a conversation it has no record of (2.1.311: "No
/// conversation found with session ID: <id>", with no `system/init` before
/// it).
const NO_CONVERSATION: &str = "No conversation found";

pub struct ClaudeHandle {
    writer: Writer,
    session_id: String,
    turn: Arc<Mutex<Option<oneshot::Sender<TurnResult>>>>,
    compat: HarnessCompat,
    /// What the pump found wrong with the `system/init` frame, for the turn
    /// that elicited it to fail with rather than with "ended mid-turn".
    fault: Fault,
}

/// The handshake refusal, held between the pump that read the init frame and
/// the prompt that is waiting on it. See `Pump::check_init`.
type Fault = Arc<Mutex<Option<AdapterError>>>;

#[async_trait]
impl HarnessHandle for ClaudeHandle {
    fn harness_session_id(&self) -> &str {
        &self.session_id
    }

    fn compat(&self) -> HarnessCompat {
        self.compat.clone()
    }

    async fn prompt(&self, text: String) -> Result<TurnResult, AdapterError> {
        self.turn(text).await
    }

    async fn compact(&self) -> Result<TurnResult, AdapterError> {
        self.turn(COMPACT_COMMAND.into()).await
    }

    async fn cancel(&self) -> Result<(), AdapterError> {
        // An interrupt is a control request like any other; the CLI advertises
        // `interrupt_receipt_v1` for it. Killing the process is the
        // supervisor's fallback and stays that.
        self.writer
            .send(json!({
                "type": "control_request",
                "request_id": format!("cancel-{}", uuid::Uuid::now_v7()),
                "request": { "subtype": "interrupt" },
            }))
            .await
    }

    async fn close(&self) -> Result<(), AdapterError> {
        // Closing stdin ends the run: there is no more input, so the harness
        // finishes what it has and exits.
        drop(self.writer.0.clone());
        Ok(())
    }
}

impl ClaudeHandle {
    /// One user message and the `result` that answers it.
    async fn turn(&self, text: String) -> Result<TurnResult, AdapterError> {
        if let Some(fault) = self.fault.lock().unwrap().take() {
            return Err(fault);
        }
        let (tx, rx) = oneshot::channel();
        *self.turn.lock().unwrap() = Some(tx);
        self.writer
            .send(json!({
                "type": "user",
                "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
            }))
            .await?;
        match rx.await {
            Ok(result) => Ok(result),
            // The first user message is what makes the CLI emit `system/init`
            // (see `launch`), so a handshake refusal surfaces here, on the
            // turn that elicited it, and not as a launch failure.
            Err(_) => Err(self
                .fault
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| AdapterError::Protocol("the harness ended mid-turn".into()))),
        }
    }
}

/// Reads the harness's stdout and turns it into the events the supervisor
/// already knows how to persist.
struct Pump {
    stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    /// Resolves when the harness process is gone. Holding it is not optional:
    /// the runner spawns with `kill_on_drop`, so dropping this future kills
    /// the harness — which looks exactly like a harness that said nothing.
    done: futures_core::future::BoxFuture<'static, Result<i32, RunnerError>>,
    writer: Writer,
    turn: Arc<Mutex<Option<oneshot::Sender<TurnResult>>>>,
    /// Permission requests still waiting on the operator, by the CLI's own
    /// request id, so a cancelled ask can be answered rather than orphaned.
    open: Arc<Mutex<HashMap<String, ()>>>,
    /// The version this node pins, checked against the init frame's own
    /// report when it arrives.
    pinned: String,
    fault: Fault,
}

fn text_of(block: &Value) -> String {
    block["text"].as_str().unwrap_or_default().to_string()
}

/// A compaction as the supervisor records one (`context_compacted`), from the
/// CLI's `compact_boundary` metadata and the summary that follows it. The
/// CLI's `manual` is the operator's `/compact`; its `auto` is the threshold.
fn compaction(metadata: &Value, summary: Option<&str>) -> Value {
    let trigger = match metadata["trigger"].as_str() {
        Some("auto") => "threshold",
        Some(other) => other,
        None => "threshold",
    };
    json!({
        "sessionUpdate": "context_compacted",
        "trigger": trigger,
        "tokensBefore": metadata["pre_tokens"],
        "tokensAfter": metadata["post_tokens"],
        "summary": summary.map(compact_summary),
    })
}

/// The summary itself, out of the message the CLI hands the model in its
/// place: without the preamble that says a summary follows, the reminder
/// block, and the closing instructions that name the transcript's path inside
/// the runner (2.1.311's wording; text it does not recognize is kept whole).
fn compact_summary(text: &str) -> String {
    let body = match text.find("</system-reminder>") {
        Some(at) => &text[at + "</system-reminder>".len()..],
        None => text
            .strip_prefix("This session is being continued from a previous conversation")
            .and_then(|rest| rest.split_once("\n\n").map(|(_, body)| body))
            .unwrap_or(text),
    };
    let body = body
        .split("\n\nIf you need specific details from before compaction")
        .next()
        .unwrap_or(body);
    body.trim().to_string()
}

impl Pump {
    /// The handshake, read off the `system/init` frame: the pin enforced a
    /// second time from the harness's own report rather than from a
    /// `--version` call that may have run against a different image (a
    /// missing version is a mismatch, not a pass), the stream-json revision
    /// these frames are decoded as, and the MCP servers — a server the
    /// harness could not reach means the session has no tools and would fail
    /// in a way that looks like the model being unhelpful.
    fn check_init(pinned: &str, init: &Value) -> Result<(), AdapterError> {
        let found = init["claude_code_version"].as_str().unwrap_or("unknown");
        if found != pinned {
            return Err(AdapterError::VersionMismatch {
                found: found.to_string(),
                pinned: pinned.to_string(),
            });
        }
        let protocol = init_protocol(init);
        if !ClaudeAdapter::PROTOCOL.accepts(protocol) {
            return Err(AdapterError::IncompatibleProtocol {
                agent: ClaudeAdapter::ID.into(),
                protocol: ClaudeAdapter::PROTOCOL.name,
                found: protocol.to_string(),
                supported: ClaudeAdapter::PROTOCOL.versions(),
            });
        }
        for s in init["mcp_servers"].as_array().into_iter().flatten() {
            if s["status"].as_str().is_some_and(|st| st != "connected") {
                return Err(AdapterError::Protocol(format!(
                    "the harness could not connect to the {} MCP server ({})",
                    s["name"].as_str().unwrap_or("tracon"),
                    s["status"].as_str().unwrap_or("unknown")
                )));
            }
        }
        Ok(())
    }

    async fn run(self, tx: mpsc::Sender<HarnessEvent>) {
        let Pump {
            stdout,
            done,
            writer,
            turn,
            open,
            pinned,
            fault,
        } = self;
        let mut lines = BufReader::new(stdout).lines();
        // Tool calls the harness has started and not asked about. Claude Code
        // decides reads and searches by its own rules and never sends a
        // `can_use_tool` for them, and the `tool_use` block always precedes
        // the ask, so a call is known to have been let through unasked only
        // when its result arrives while it is still here.
        let mut unasked: HashMap<String, (String, Value)> = HashMap::new();
        // A `compact_boundary` whose summary has not arrived yet.
        let mut compacting: Option<Value> = None;
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match v["type"].as_str().unwrap_or_default() {
                "system" if v["subtype"] == "init" => {
                    if let Err(refused) = Self::check_init(&pinned, &v) {
                        tracing::error!(error = %refused, "harness handshake refused");
                        *fault.lock().unwrap() = Some(refused);
                        // The turn that elicited the frame fails with the
                        // fault; the process is ended rather than driven.
                        drop(turn.lock().unwrap().take());
                        drop(done);
                        let _ = tx.send(HarnessEvent::Exited { code: None }).await;
                        return;
                    }
                    let _ = tx.send(HarnessEvent::Other(v.clone())).await;
                }
                "assistant" => {
                    let id = v["message"]["id"].as_str().map(str::to_string);
                    for block in v["message"]["content"].as_array().into_iter().flatten() {
                        match block["type"].as_str().unwrap_or_default() {
                            "text" => {
                                let _ = tx
                                    .send(HarnessEvent::MessageChunk {
                                        message_id: id.clone(),
                                        text: text_of(block),
                                    })
                                    .await;
                            }
                            "thinking" => {
                                let _ = tx
                                    .send(HarnessEvent::ThoughtChunk {
                                        message_id: id.clone(),
                                        text: block["thinking"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .to_string(),
                                    })
                                    .await;
                            }
                            "tool_use" | "mcp_tool_use" | "server_tool_use" => {
                                // A server tool runs at the provider and its
                                // result never comes back as a `tool_result`.
                                if block["type"] != "server_tool_use" {
                                    if let Some(id) = block["id"].as_str() {
                                        unasked.insert(
                                            id.to_string(),
                                            (
                                                block["name"]
                                                    .as_str()
                                                    .unwrap_or_default()
                                                    .to_string(),
                                                block["input"].clone(),
                                            ),
                                        );
                                    }
                                }
                                let _ = tx
                                    .send(HarnessEvent::ToolCall(ToolCall {
                                        tool_call_id: block["id"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .to_string(),
                                        title: block["name"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .to_string(),
                                        kind: Some("other".into()),
                                        status: Some("in_progress".into()),
                                        raw_input: Some(block["input"].clone()),
                                        content: Vec::new(),
                                        locations: Vec::new(),
                                        parent_tool_call_id: None,
                                    }))
                                    .await;
                            }
                            _ => {}
                        }
                    }
                }
                // The CLI compacted the conversation, on its own or for a
                // `/compact`: the summary is the user message that follows.
                "system" if v["subtype"] == "compact_boundary" => {
                    compacting = Some(v["compact_metadata"].clone());
                }
                // The summary the model works from now. The CLI's own echo
                // of the command (`<local-command-stdout>`) follows it.
                "user"
                    if compacting.is_some()
                        && v["message"]["content"]
                            .as_str()
                            .is_some_and(|text| !text.starts_with("<local-command")) =>
                {
                    if let Some(metadata) = compacting.take() {
                        let summary = v["message"]["content"].as_str();
                        let _ = tx
                            .send(HarnessEvent::Other(compaction(&metadata, summary)))
                            .await;
                    }
                }
                // A tool's result comes back as a synthetic user message.
                "user" => {
                    for block in v["message"]["content"].as_array().into_iter().flatten() {
                        if block["type"] == "tool_result" {
                            let id = block["tool_use_id"].as_str().unwrap_or_default();
                            if let Some((tool, input)) = unasked.remove(id) {
                                let _ = tx
                                    .send(HarnessEvent::Decided(decided(id, &tool, input)))
                                    .await;
                            }
                            let _ = tx
                                .send(HarnessEvent::ToolCallUpdate(tool_result_update(block)))
                                .await;
                        }
                    }
                }
                "control_request" => {
                    let id = v["request_id"].as_str().unwrap_or_default().to_string();
                    if v["request"]["subtype"] != "can_use_tool" {
                        continue;
                    }
                    let tool = v["request"]["tool_name"]
                        .as_str()
                        .unwrap_or("a tool")
                        .to_string();
                    let input = v["request"]["input"].clone();
                    if let Some(id) = v["request"]["tool_use_id"].as_str() {
                        unasked.remove(id);
                    }
                    let (reply_tx, reply_rx) = oneshot::channel();
                    open.lock().unwrap().insert(id.clone(), ());
                    let _ = tx
                        .send(HarnessEvent::Permission {
                            request: {
                                let (resource, command) = permission_target(&tool, &input);
                                PermissionRequest::managed(
                                    v["request"]["tool_use_id"].as_str().map(str::to_string),
                                    summarize(&tool, &input),
                                    &tool,
                                    resource,
                                    command,
                                    Some(input),
                                    options(),
                                )
                            },
                            reply: reply_tx,
                        })
                        .await;
                    // The answer may be minutes away; waiting for it here would
                    // stop reading the stream, so it is awaited on its own.
                    let w = writer.clone();
                    let open = open.clone();
                    tokio::spawn(async move {
                        let decision = reply_rx.await.unwrap_or(PermissionReply::Cancelled);
                        open.lock().unwrap().remove(&id);
                        let _ = w.send(control_response(&id, decision)).await;
                    });
                }
                "result" => {
                    // A turn's calls have all answered by its end; one that
                    // never did (an interrupt) has nothing left to record.
                    unasked.clear();
                    if let Some(metadata) = compacting.take() {
                        let _ = tx
                            .send(HarnessEvent::Other(compaction(&metadata, None)))
                            .await;
                    }
                    // A reopening whose conversation the CLI has no record
                    // of: the turn fails saying so, not as a turn in error.
                    if let Some(gone) = v["errors"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .find(|e| e.contains(NO_CONVERSATION))
                    {
                        *fault.lock().unwrap() = Some(AdapterError::ConversationGone(format!(
                            "Claude Code has no record of this conversation to resume ({gone})"
                        )));
                        drop(turn.lock().unwrap().take());
                        continue;
                    }
                    let usage = usage_of(&v);
                    let _ = tx
                        .send(HarnessEvent::Usage {
                            size: None,
                            used: Some(usage.charged()),
                            cost_usd: v["total_cost_usd"].as_f64(),
                        })
                        .await;
                    let stop = match v["subtype"].as_str().or_else(|| v["status"].as_str()) {
                        Some("success") | None => "end_turn",
                        Some("error_max_turns") => "max_turn_requests",
                        Some(_) => "error",
                    };
                    if let Some(done) = turn.lock().unwrap().take() {
                        let _ = done.send(TurnResult {
                            stop_reason: stop.into(),
                            usage,
                        });
                    }
                }
                // `api_retry` is worth keeping: it is why a turn is slow.
                "system" if v["subtype"] == "api_retry" => {
                    let _ = tx.send(HarnessEvent::Other(v.clone())).await;
                }
                _ => {}
            }
        }
        // The stream ended. A turn still waiting will never be answered, so let
        // it fail rather than hang the session forever.
        drop(turn.lock().unwrap().take());
        let code = done.await.ok();
        let _ = tx.send(HarnessEvent::Exited { code }).await;
    }
}

fn control_response(id: &str, decision: PermissionReply) -> Value {
    let inner = match decision {
        PermissionReply::Selected(opt) if opt == types::OPTION_ALLOW_ONCE => {
            json!({ "behavior": "allow" })
        }
        // Edits are refused before they could reach a harness's own request.
        PermissionReply::Selected(_) | PermissionReply::Edited { .. } => json!({
            "behavior": "deny",
            "message": "the operator declined this",
        }),
        PermissionReply::Refused { rule, reason } => json!({
            "behavior": "deny",
            "message": refusal_message(rule.as_deref(), reason.as_deref()),
        }),
        PermissionReply::Cancelled => json!({
            "behavior": "deny",
            "message": "no answer was given before this expired",
        }),
    };
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": id, "response": inner },
    })
}

/// What the agent reads when the node's policy, not the operator, refused a
/// call: whose decision it was, the rule, and the rule's own reason.
fn refusal_message(rule: Option<&str>, reason: Option<&str>) -> String {
    let mut message = String::from("the node's policy refused this");
    if let Some(rule) = rule.filter(|r| !r.is_empty()) {
        message.push_str(&format!(" (rule `{rule}`)"));
    }
    if let Some(reason) = reason.filter(|r| !r.is_empty()) {
        message.push_str(": ");
        message.push_str(reason);
    }
    message
}

fn usage_of(v: &Value) -> Usage {
    let u = &v["usage"];
    let input = u["input_tokens"]
        .as_u64()
        .or_else(|| v["total_input_tokens"].as_u64())
        .unwrap_or(0);
    let output = u["output_tokens"]
        .as_u64()
        .or_else(|| v["total_output_tokens"].as_u64())
        .unwrap_or(0);
    let cached = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let written = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    Usage {
        input_tokens: input,
        output_tokens: output,
        // The CLI reports no total. Anthropic's `input_tokens` leaves the
        // cache out, so the total is all four, and `charged()` takes the
        // reads back out.
        total_tokens: input + output + cached + written,
        cached_read_tokens: cached,
        cache_write_tokens: written,
    }
}

/// A `tool_result` block as the terminal update of its tool call. The ledger
/// records `raw_output`, so it carries the result as text: Claude Code sends
/// either a string or an array of content blocks.
fn tool_result_update(block: &Value) -> ToolCallUpdate {
    let output = match &block["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part["text"].as_str() {
                Some(text) if part["type"] == "text" => text.to_string(),
                _ => format!("[{}]", part["type"].as_str().unwrap_or("content")),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    ToolCallUpdate {
        tool_call_id: block["tool_use_id"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        status: Some(if block["is_error"].as_bool().unwrap_or(false) {
            "failed".into()
        } else {
            "completed".into()
        }),
        kind: None,
        title: None,
        content: vec![block["content"].clone()],
        raw_output: Some(Value::String(output)),
    }
}

/// The request a call the harness ran unasked would have made, normalized
/// exactly as a `can_use_tool` for it is, so the policy reads one subject
/// whether the harness asked or not.
fn decided(id: &str, tool: &str, input: Value) -> PermissionRequest {
    let (resource, command) = permission_target(tool, &input);
    PermissionRequest::managed(
        Some(id.to_string()),
        summarize(tool, &input),
        tool,
        resource,
        command,
        Some(input),
        Vec::new(),
    )
}

/// What the operator reads in the queue. The tool's own name plus the one
/// field that says what it would do, which for a shell is the command.
fn summarize(tool: &str, input: &Value) -> String {
    for key in ["command", "file_path", "path", "url", "pattern"] {
        if let Some(v) = input[key].as_str() {
            let one = v.replace('\n', " ");
            let short: String = one.chars().take(160).collect();
            return format!("{tool}: {short}");
        }
    }
    tool.to_string()
}

/// The exact target policy evaluates, separate from the title shown on a
/// permission card. Unknown tools get neither and therefore cannot inherit a
/// read or execute exemption merely because their prose resembles one.
fn permission_target(tool: &str, input: &Value) -> (Option<String>, Option<String>) {
    let value = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| input[*key].as_str())
            .map(str::to_string)
    };
    let action = tool.trim().to_ascii_lowercase();
    if action.as_bytes() == b"note\x62ookedit" || action.as_bytes() == b"note\x62ook_edit" {
        return (value(&["note\x62ook_path", "file_path"]), None);
    }
    match action.as_str() {
        "bash" => (None, value(&["command"])),
        "read" | "write" | "edit" => (value(&["file_path"]), None),
        "glob" | "grep" | "list" | "ls" => (value(&["path", "pattern"]), None),
        _ => (None, None),
    }
}

#[async_trait]
impl HarnessAdapter for ClaudeAdapter {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn pinned_version(&self) -> &str {
        &self.pinned
    }

    fn protocol(&self) -> ProtocolSupport {
        Self::PROTOCOL
    }

    fn layout(&self) -> Layout {
        Self::layout()
    }

    /// The plugin `--plugin-dir` names: a manifest and the channel's skills.
    /// Staged even when there are no skills, so the directory the argv names
    /// always exists. And the MCP configuration `--mcp-config` names, when
    /// the session is offered the node's tools.
    fn scratch_files(&self, wiring: &crate::gateway::model::Wiring) -> Vec<(String, String)> {
        let manifest = json!({
            "name": "tracon",
            "version": "1.0.0",
            "description": "The skills this channel's launch manifest installed.",
        });
        let mut files = vec![(
            format!("{}/.claude-plugin/plugin.json", Self::PLUGIN_DIR),
            serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".into()),
        )];
        files.extend(
            wiring
                .manifest
                .skill_files()
                .into_iter()
                .map(|(rel, text)| (format!("{}/{rel}", Self::PLUGIN_DIR), text)),
        );
        if !wiring.mcp_servers.is_empty() {
            files.push((
                Self::MCP_CONFIG_FILE.to_string(),
                mcp_config(&wiring.mcp_servers).to_string(),
            ));
        }
        files
    }

    fn readonly_dirs(&self) -> Vec<String> {
        vec![Self::PLUGIN_DIR.to_string()]
    }

    /// The CLI records each conversation under its state directory, which is
    /// the node-wide harness state volume rather than the session's scratch
    /// (`materialize::state_mounts`), so the record outlives the container.
    /// `--resume <id>` in stream-json mode carries it on under the same id
    /// (seen from 2.1.311).
    fn reopens(&self) -> bool {
        true
    }

    fn compacts(&self) -> bool {
        true
    }

    /// The CLI keeps a conversation at `projects/<cwd, escaped>/<id>.jsonl`
    /// under its state directory. Looked for under any project directory,
    /// since the CLI finds it there from any working directory too. Only a
    /// plain "not there" says it is gone: a shell that would not run, or a
    /// state directory with no `projects` at all (not mounted where the
    /// probe looked), says nothing, and the CLI is left to say it.
    async fn has_conversation(
        &self,
        runner: &dyn Runner,
        spec: &LaunchSpec,
        harness_session_id: &str,
    ) -> Result<bool, AdapterError> {
        let projects = format!(
            "{}/projects",
            crate::session::materialize::state_target(&spec.harness_home, Self::layout())
        );
        let out = runner
            .run_probe(RunnerCommand {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    r#"[ -d "$1" ] || exit 2; for f in "$1"/*/"$2".jsonl; do [ -f "$f" ] && exit 0; done; exit 1"#.into(),
                    "sh".into(),
                    projects,
                    harness_session_id.into(),
                ],
                name: format!("{}-conversation", spec.container_name),
                ..Default::default()
            })
            .await?;
        Ok(out.status.code() != Some(1))
    }

    /// `--setting-sources project` is what loads the repository's `CLAUDE.md`,
    /// and it loads `.claude/settings.json` with it: permission rules that
    /// allow tools without the node seeing the ask, and hooks and a status
    /// line that run commands. `settings.local.json` is not a project source.
    fn refused_workspace_entries(&self) -> &'static [&'static str] {
        &[".claude/settings.json"]
    }

    async fn version(&self, runner: &dyn Runner) -> Result<HarnessVersion, AdapterError> {
        let out = runner
            .run_probe(RunnerCommand {
                argv: vec![Self::BINARY.into(), "--version".into()],
                name: "claude-version".into(),
                ..Default::default()
            })
            .await?;
        Ok(HarnessVersion {
            found: parse_version(&String::from_utf8_lossy(&out.stdout)),
            pinned: self.pinned.clone(),
        })
    }

    /// Claude Code has no model catalogue to ask for, so this reports what
    /// the node declared instead of probing anything: nothing is launched,
    /// since the pinned CLI emits its `system/init` frame only after a first
    /// user message, and a user message is a model call.
    ///
    /// Claude Code only ever talks to Anthropic, so the one `anthropic`-shaped
    /// provider's `models` is the whole answer when it names any — mapped the
    /// same way `native.rs` maps a declaring harness's models to
    /// `ModelOption`, minus the `provider/` prefix that addressing needs
    /// there: `--model` forwards a bare id here. A provider that names
    /// none, or an anthropic-shaped provider not found at all, falls back to
    /// the fixed alias list unchanged.
    async fn probe_models(
        &self,
        _runner: &dyn Runner,
        wiring: &crate::gateway::model::Wiring,
    ) -> Result<Vec<ModelOption>, AdapterError> {
        let declared = wiring
            .providers
            .iter()
            .find(|p| p.shape == crate::config::SHAPE_ANTHROPIC)
            .filter(|p| !p.models.is_empty())
            .map(|p| {
                p.models
                    .iter()
                    .map(|model| ModelOption {
                        value: model.id.clone(),
                        name: model.label().to_string(),
                        harnesses: Vec::new(),
                    })
                    .collect::<Vec<_>>()
            });
        Ok(declared.unwrap_or_else(|| {
            ALIASES
                .iter()
                .map(|a| ModelOption {
                    value: (*a).to_string(),
                    name: (*a).to_string(),
                    harnesses: Vec::new(),
                })
                .collect()
        }))
    }

    async fn launch(
        &self,
        runner: &dyn Runner,
        spec: LaunchSpec,
    ) -> Result<(Box<dyn HarnessHandle>, mpsc::Receiver<HarnessEvent>), AdapterError> {
        // The node picks the session id rather than discovering it, so the row
        // it has already written and the harness's own id are the same string
        // even if the handshake fails. A reopened session keeps the one its
        // conversation was recorded under.
        let session_id = match &spec.resume {
            Some(resume) => resume.harness_session_id.clone(),
            None => uuid::Uuid::now_v7().to_string(),
        };
        let mut spec = spec;
        spec.model = claude_model(&spec.model)?.to_string();
        let Spawned {
            stdin,
            stdout,
            done,
            ..
        } = runner
            .spawn(Self::cmd(&spec.container_name, &spec, &session_id))
            .await?;
        let writer = Writer::spawn(stdin);

        // Nothing is read here. The pinned CLI emits `system/init` only once
        // the first user message has arrived on stdin (observed against
        // 2.1.247 and 2.1.295: with stdin open and silent it prints nothing
        // for as long as it is left alone), so a launch that waited for the
        // frame before writing anything waited forever. The frame is checked
        // by the pump when the first turn elicits it (`Pump::check_init`), and
        // that turn is what fails if the harness is not the one this node
        // pins. Until then the handle reports the pin, which `version()`
        // already checked against the image at startup.
        let fault: Fault = Arc::new(Mutex::new(None));
        let turn = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel(256);
        let handle = ClaudeHandle {
            writer: writer.clone(),
            session_id,
            turn: turn.clone(),
            compat: HarnessCompat {
                agent: Self::ID.into(),
                version: self.pinned.clone(),
                protocol: Self::PROTOCOL.tag(Self::PROTOCOL.max),
            },
            fault: fault.clone(),
        };
        tokio::spawn(
            Pump {
                stdout,
                done,
                writer,
                turn,
                open: Arc::new(Mutex::new(HashMap::new())),
                pinned: self.pinned.clone(),
                fault,
            }
            .run(tx),
        );
        Ok((Box::new(handle), rx))
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_model_reaches_claude_code_as_anthropic_names_it() {
        assert_eq!(
            claude_model("anthropic/claude-opus-5").unwrap(),
            "claude-opus-5"
        );
        assert_eq!(claude_model("claude-opus-5").unwrap(), "claude-opus-5");
        assert_eq!(claude_model("sonnet").unwrap(), "sonnet");
        let refused = claude_model("openai-codex/gpt-5.5")
            .unwrap_err()
            .to_string();
        assert!(refused.contains("Anthropic models only"), "{refused}");
    }
    use super::*;

    fn spec() -> LaunchSpec {
        LaunchSpec {
            secret_env: Default::default(),
            cwd_in_runner: "/work".into(),
            model: "opus".into(),
            container_name: "c".into(),
            harness_home: "/root".into(),
            mcp_servers: vec![json!({
                "type": "http",
                "name": "tracon",
                "url": "http://gw:7421/mcp/s1",
                "headers": [{ "name": "Authorization", "value": "Bearer tok" }],
            })],
            tools: Vec::new(),
            env: vec![(
                "ANTHROPIC_BASE_URL".into(),
                "http://gw/model/anthropic".into(),
            )],
            system_prompt_file: Some("/root/.claude/orientation.md".into()),
            resume: None,
        }
    }

    /// The two flags that decide whether the node sees a permission request at
    /// all. `dontAsk` or `bypassPermissions` here would let the harness answer
    /// its own tool calls and the queue would simply stay empty.
    #[test]
    fn the_harness_is_launched_asking_for_permission() {
        let cmd = ClaudeAdapter::cmd("c", &spec(), "sid");
        let argv = cmd.argv.join(" ");
        assert!(argv.contains("--permission-mode default"), "{argv}");
        assert!(argv.contains("--permission-prompt-tool stdio"), "{argv}");
        assert!(!argv.contains("bypassPermissions"), "{argv}");
        assert!(!argv.contains("dontAsk"), "{argv}");
        assert!(argv.contains("--strict-mcp-config"), "{argv}");
        assert!(
            cmd.env
                .iter()
                .any(|(name, value)| name == "BUN_OPTIONS" && value.is_empty()),
            "BUN_OPTIONS is not stated empty"
        );
        assert!(argv.contains("--input-format stream-json"), "{argv}");
        assert!(argv.contains("--output-format stream-json"), "{argv}");
        assert!(argv.contains("--session-id sid"), "{argv}");
        assert!(argv.contains("--model opus"), "{argv}");
        assert!(
            argv.contains("--append-system-prompt-file /root/.claude/orientation.md"),
            "{argv}"
        );
    }

    /// `--allowedTools` removes nothing, so the session is offered only the
    /// built-ins a node session uses: `Skill` for the manifest's skills, and
    /// never `AskUserQuestion`, which nobody can answer headless.
    #[test]
    fn the_harness_is_offered_only_the_built_in_tools_a_node_session_uses() {
        let argv = ClaudeAdapter::cmd("c", &spec(), "sid").argv;
        let at = argv.iter().position(|a| a == "--tools").expect("--tools");
        let offered: Vec<&str> = argv[at + 1].split(',').collect();
        assert_eq!(
            offered,
            [
                "Bash",
                "Read",
                "Edit",
                "Write",
                "TaskStop",
                "Skill",
                "ToolSearch"
            ]
        );
        assert!(!offered.contains(&"AskUserQuestion"));

        // An operator's own list decides, by tool name, without the node's
        // MCP tools, which are not built-ins.
        let mut configured = spec();
        configured.tools = vec![
            "Bash(git:*)".into(),
            "Bash".into(),
            "Read".into(),
            "mcp__tracon__recall".into(),
        ];
        let argv = ClaudeAdapter::cmd("c", &configured, "sid").argv;
        let at = argv.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(argv[at + 1], "Bash,Read,ToolSearch");
        let at = argv.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(argv[at + 1], "Bash(git:*),Bash,Read,mcp__tracon__recall");
    }

    /// The node's tools are deferred behind tool search, which takes both the
    /// built-in and the variable: either alone loads every definition up
    /// front behind the gateway.
    #[test]
    fn the_nodes_tools_are_deferred_behind_tool_search() {
        let cmd = ClaudeAdapter::cmd("c", &spec(), "sid");
        assert!(
            cmd.env
                .iter()
                .any(|(name, value)| name == "ENABLE_TOOL_SEARCH" && value == "true"),
            "{:?}",
            cmd.env
        );
        let mut configured = spec();
        configured.tools = vec!["Bash".into(), "ToolSearch".into()];
        let argv = ClaudeAdapter::cmd("c", &configured, "sid").argv;
        let at = argv.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(argv[at + 1], "Bash,ToolSearch", "named once");
    }

    /// The manifest's skills reach Claude Code as a sealed plugin the argv
    /// names, under the state directory rather than in the worktree.
    #[test]
    fn the_manifests_skills_are_staged_as_a_plugin_the_argv_names() {
        let argv = ClaudeAdapter::cmd("c", &spec(), "sid").argv.join(" ");
        assert!(argv.contains("--plugin-dir /root/.claude/plugin"), "{argv}");

        let adapter = ClaudeAdapter::new(ClaudeAdapter::PINNED_VERSION);
        assert_eq!(adapter.readonly_dirs(), ["plugin"]);
        let mut wiring = crate::gateway::model::Wiring::default();
        wiring.manifest.skills = crate::manifest::builtin_skills();
        let files = adapter.scratch_files(&wiring);
        let paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "plugin/.claude-plugin/plugin.json",
                "plugin/skills/repo-setup/SKILL.md"
            ]
        );
        let plugin: Value = serde_json::from_str(&files[0].1).unwrap();
        assert_eq!(plugin["name"], "tracon");

        // No skills still stages the manifest, so the named directory exists.
        let empty = adapter.scratch_files(&crate::gateway::model::Wiring::default());
        assert_eq!(empty.len(), 1);
    }

    /// The node builds one neutral descriptor for every adapter; this one has
    /// to become the map Claude Code expects, with headers as an object.
    #[test]
    fn the_mcp_server_is_rendered_the_way_this_harness_wants_it() {
        let cfg = mcp_config(&spec().mcp_servers);
        let server = &cfg["mcpServers"]["tracon"];
        assert_eq!(server["type"], "http");
        assert_eq!(server["url"], "http://gw:7421/mcp/s1");
        assert_eq!(server["headers"]["Authorization"], "Bearer tok");
        // Past the client's 60 s default, and past 300 s so the pinned release
        // turns Bun's own fetch timeout off; tools that wait on a person
        // still return within MAX_WAIT_SECS.
        assert_eq!(server["timeout"], MCP_CALL_TIMEOUT_MS);
        const { assert!(MCP_CALL_TIMEOUT_MS > 300_000) };
    }

    /// The server's header carries the session's token, which an argv shows
    /// to `ps` and a pod spec to anyone who may read pods: the argv names a
    /// staged file instead, and the file holds the configuration.
    #[test]
    fn the_mcp_token_is_staged_in_a_file_and_never_on_the_argv() {
        let cmd = ClaudeAdapter::cmd("c", &spec(), "sid");
        let argv = cmd.argv.join(" ");
        assert!(!argv.contains("tok"), "{argv}");
        assert!(
            argv.contains("--mcp-config /root/.claude/tracon-mcp.json"),
            "{argv}"
        );
        let wiring = crate::gateway::model::Wiring {
            mcp_servers: spec().mcp_servers,
            ..Default::default()
        };
        let files = ClaudeAdapter::new(ClaudeAdapter::PINNED_VERSION).scratch_files(&wiring);
        let (_, staged) = files
            .iter()
            .find(|(path, _)| path == "tracon-mcp.json")
            .expect("the MCP configuration is staged");
        let staged: Value = serde_json::from_str(staged).unwrap();
        assert_eq!(staged, mcp_config(&spec().mcp_servers));
    }

    /// The ledger keeps what a tool returned, whichever shape Claude Code sent
    /// it in, and a failure keeps its message.
    #[test]
    fn a_tool_result_carries_its_output_to_the_ledger() {
        let plain = tool_result_update(&json!({
            "type": "tool_result",
            "tool_use_id": "t1",
            "content": "b95f6b6 fix(spa): leave a verdict's navigation",
            "is_error": false,
        }));
        assert_eq!(plain.tool_call_id, "t1");
        assert_eq!(plain.status.as_deref(), Some("completed"));
        assert_eq!(
            plain.raw_output,
            Some(json!("b95f6b6 fix(spa): leave a verdict's navigation"))
        );

        let blocks = tool_result_update(&json!({
            "type": "tool_result",
            "tool_use_id": "t2",
            "content": [
                { "type": "text", "text": "232 pass" },
                { "type": "image", "source": {} },
                { "type": "text", "text": "0 fail" },
            ],
        }));
        assert_eq!(blocks.raw_output, Some(json!("232 pass\n[image]\n0 fail")));

        let failed = tool_result_update(&json!({
            "type": "tool_result",
            "tool_use_id": "t3",
            "content": "Error: worktree is for a harness you run yourself",
            "is_error": true,
        }));
        assert_eq!(failed.status.as_deref(), Some("failed"));
        assert_eq!(
            failed.raw_output,
            Some(json!("Error: worktree is for a harness you run yourself"))
        );
    }

    /// A reopened session names its own conversation with `--resume`, never
    /// `--session-id` beside it, which the CLI refuses.
    #[test]
    fn a_reopened_session_resumes_its_conversation_by_id() {
        let mut resumed = spec();
        resumed.resume = Some(crate::adapter::Resume {
            harness_session_id: "sid".into(),
            last_seq: 0,
        });
        let argv = ClaudeAdapter::cmd("c", &resumed, "sid").argv;
        let at = argv.iter().position(|a| a == "--resume").expect("--resume");
        assert_eq!(argv[at + 1], "sid");
        assert!(!argv.iter().any(|a| a == "--session-id"), "{argv:?}");
        assert!(!argv.iter().any(|a| a == "--fork-session"), "{argv:?}");
        let fresh = ClaudeAdapter::cmd("c", &spec(), "sid").argv;
        assert!(!fresh.iter().any(|a| a == "--resume"), "{fresh:?}");
    }

    /// The CLI's compaction reaches the supervisor as the event automatic
    /// compaction is recorded from, with the summary and not the
    /// instructions wrapped around it.
    #[test]
    fn a_compaction_is_told_with_its_summary_alone() {
        let text = "This session is being continued from a previous conversation that ran out \
                    of context. The summary below covers the earlier portion of the \
                    conversation.\n\n<system-reminder>\n<tools-used-before-summary>\
                    </tools-used-before-summary>\n</system-reminder>\nThe ledger is half \
                    done.\n\nIf you need specific details from before compaction (like exact \
                    code snippets), read the full transcript at: /root/.claude/projects/-work/\
                    s.jsonl\nContinue the conversation from where it left off.";
        let event = compaction(
            &json!({ "trigger": "manual", "pre_tokens": 48_000, "post_tokens": 3_100 }),
            Some(text),
        );
        assert_eq!(event["sessionUpdate"], "context_compacted");
        assert_eq!(event["trigger"], "manual");
        assert_eq!(event["tokensBefore"], 48_000);
        assert_eq!(event["tokensAfter"], 3_100);
        assert_eq!(event["summary"], "The ledger is half done.");

        let auto = compaction(&json!({ "trigger": "auto" }), None);
        assert_eq!(auto["trigger"], "threshold");
        assert!(auto["summary"].is_null());
        // Wording it does not know is kept whole rather than guessed at.
        assert_eq!(compact_summary("  just a summary "), "just a summary");
    }

    #[test]
    fn a_version_is_read_out_of_the_cli_banner() {
        assert_eq!(parse_version("2.1.295 (Claude Code)\n"), "2.1.295");
        assert_eq!(parse_version("something odd"), "unknown");
        assert_eq!(parse_version(""), "unknown");
    }

    #[test]
    fn an_allow_and_a_deny_become_the_shapes_the_protocol_defines() {
        let allow = control_response(
            "r1",
            PermissionReply::Selected(types::OPTION_ALLOW_ONCE.into()),
        );
        assert_eq!(allow["response"]["request_id"], "r1");
        assert_eq!(allow["response"]["response"]["behavior"], "allow");

        let deny = control_response(
            "r2",
            PermissionReply::Selected(types::OPTION_REJECT_ONCE.into()),
        );
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
        assert!(deny["response"]["response"]["message"].is_string());

        // An expiry is a deny too: never a silent allow.
        let cancelled = control_response("r3", PermissionReply::Cancelled);
        assert_eq!(cancelled["response"]["response"]["behavior"], "deny");
    }

    /// The agent is told whose decision a deny was: the operator's answer on
    /// a card, or the node's policy with the rule and its reason.
    #[test]
    fn a_policy_refusal_is_not_told_as_the_operators() {
        let operator = control_response(
            "r1",
            PermissionReply::Selected(types::OPTION_REJECT_ONCE.into()),
        );
        assert_eq!(
            operator["response"]["response"]["message"],
            "the operator declined this"
        );

        let policy = control_response(
            "r2",
            PermissionReply::Refused {
                rule: Some("harness-settings".into()),
                reason: Some("The harness's settings are the node's, not the session's.".into()),
            },
        );
        assert_eq!(policy["response"]["response"]["behavior"], "deny");
        assert_eq!(
            policy["response"]["response"]["message"],
            "the node's policy refused this (rule `harness-settings`): The harness's settings are the node's, not the session's."
        );

        let bare = control_response(
            "r3",
            PermissionReply::Refused {
                rule: None,
                reason: None,
            },
        );
        assert_eq!(
            bare["response"]["response"]["message"],
            "the node's policy refused this"
        );
    }

    /// An unrecognised option id must not read as an allow. Anything that is
    /// not the allow id denies.
    #[test]
    fn an_unknown_option_denies() {
        let v = control_response("r", PermissionReply::Selected("allow_always".into()));
        assert_eq!(v["response"]["response"]["behavior"], "deny");
    }

    #[test]
    fn a_permission_title_says_what_the_tool_would_do() {
        assert_eq!(
            summarize("Bash", &json!({ "command": "git push origin main" })),
            "Bash: git push origin main"
        );
        assert_eq!(
            summarize("Edit", &json!({ "file_path": "/work/src/a.rs" })),
            "Edit: /work/src/a.rs"
        );
        assert_eq!(summarize("Task", &json!({})), "Task");
        // A multi-line command is one line in a queue.
        assert!(!summarize("Bash", &json!({ "command": "a\nb" })).contains('\n'));
    }

    #[test]
    fn usage_is_charged_even_though_the_cli_reports_no_total() {
        let u = usage_of(&json!({
            "usage": {
                "input_tokens": 100, "output_tokens": 20,
                "cache_read_input_tokens": 5_000, "cache_creation_input_tokens": 30
            }
        }));
        assert_eq!(u.total_tokens, 5_150);
        assert_eq!(
            u.charged(),
            150,
            "cache writes are charged, cache reads are not"
        );
    }

    #[test]
    fn a_repository_that_would_grant_the_harness_tools_is_refused() {
        let refused = ClaudeAdapter::new("1").refused_workspace_entries();
        assert_eq!(refused, &[".claude/settings.json"]);
        assert!(
            !refused.iter().any(|e| e.contains("local")),
            "settings.local.json is not a project source and a checkout may keep one"
        );
    }

    #[test]
    fn the_state_directory_is_this_harnesss_own() {
        let l = ClaudeAdapter::layout();
        assert_eq!(l.dir, ".claude");
        assert_eq!(l.env, "CLAUDE_CONFIG_DIR");
    }
}
