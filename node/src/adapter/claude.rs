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
//! control protocol nor `--permission-prompt-tool`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::{
    AdapterError, HarnessAdapter, HarnessCompat, HarnessEvent, HarnessHandle, HarnessVersion,
    LaunchSpec, Layout, LiftedToken, LoginFlow, ModelOption, PermissionReply, PermissionRequest,
    ProtocolSupport, TurnResult,
};
use crate::adapter::types::{self, PermissionOption, ToolCall, ToolCallUpdate, Usage};
use crate::runner::{Runner, RunnerCommand, RunnerError, Spawned};

/// How long to wait for the `system/init` frame before giving up on a launch.
/// It is emitted before any model call, so this only has to cover process
/// start inside a container.
const INIT_TIMEOUT_SECS: u64 = 60;

/// The models this adapter offers. Claude Code has no catalogue endpoint of
/// its own; these are the aliases it resolves to whatever is current, which is
/// also what keeps a pinned node from silently following a model change.
const ALIASES: &[&str] = &["opus", "sonnet", "haiku"];

/// How long to wait for `claude setup-token` to print its sign-in URL.
const LOGIN_URL_TIMEOUT: Duration = Duration::from_secs(90);

/// How long the login is given to end on its own once the token is in hand.
const LOGIN_EXIT_GRACE: Duration = Duration::from_secs(10);

/// How long a pasted code is left to land before Enter follows it. 2.1.247
/// submitted with a 50 ms gap and did not with none.
const PASTE_SETTLE: Duration = Duration::from_millis(250);

/// The width the login's pty is set to before the CLI starts. At the default
/// 80 the sign-in URL is wrapped across rows by cursor moves and no line
/// contains it whole; wide enough, it is printed once, on one line.
const LOGIN_COLUMNS: u16 = 400;

/// What a `claude setup-token` token looks like. Long-lived subscription
/// tokens carry this prefix, which is what distinguishes the token from every
/// other string on the login's screen.
const TOKEN_PREFIX: &str = "sk-ant-oat";

/// The lifetime assumed when the CLI does not say. `setup-token` calls itself
/// a "long-lived (1-year)" token in its own banner, and the node only uses
/// this to decide when to warn; the real expiry is the API's.
const DEFAULT_TOKEN_DAYS: i64 = 365;

const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

pub struct ClaudeAdapter {
    pinned: String,
    /// The image `claude setup-token` runs in when the node's own harness is
    /// not Claude Code. `None` leaves the runner's harness image, which is
    /// right when this adapter is also the session harness.
    login_image: Option<String>,
    /// What a completed login printed, by provider.
    ///
    /// `setup-token` prints its token once and stores nothing: its own screen
    /// says "you won't be able to see it again". So unlike every other lift
    /// there is no store directory to read afterwards, and the adapter
    /// instance that ran the login is the only place the token exists until
    /// the broker has it. Taken by `lift`, so it is not held any longer.
    minted: Arc<Mutex<HashMap<String, LiftedToken>>>,
}

impl ClaudeAdapter {
    pub const ID: &'static str = "claude";

    /// The one provider this harness can log in to. Claude Code mints an
    /// Anthropic subscription token and nothing else.
    pub const LOGIN_PROVIDER: &'static str = "anthropic";

    /// The version this node's harness image installs.
    /// `containers/harness-claude/Containerfile` fetches exactly this release
    /// at exactly one digest; `the_image_installs_the_pinned_claude` keeps the
    /// two from drifting apart.
    pub const PINNED_VERSION: &'static str = "2.1.247";

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
            login_image: None,
            minted: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Run the login helper in `image` rather than in whatever harness image
    /// the runner carries. This is what lets a node whose sessions run another
    /// harness still sign in to an Anthropic subscription.
    pub fn with_login_image(mut self, image: Option<String>) -> Self {
        self.login_image = image;
        self
    }

    pub const fn layout() -> Layout {
        Layout {
            dir: ".claude",
            env: "CLAUDE_CONFIG_DIR",
        }
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
            "claude".into(),
            "--print".into(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            "default".into(),
            "--strict-mcp-config".into(),
            // Never read the operator's own settings or memory: the boundary
            // gives it a fresh home, and this makes that explicit rather than
            // incidental.
            "--setting-sources".into(),
            "project".into(),
            "--session-id".into(),
            session_id.into(),
            "--add-dir".into(),
            spec.cwd_in_runner.clone(),
            "--model".into(),
            spec.model.clone(),
        ];
        if !spec.mcp_servers.is_empty() {
            argv.push("--mcp-config".into());
            argv.push(mcp_config(&spec.mcp_servers).to_string());
        }
        if let Some(file) = &spec.system_prompt_file {
            argv.push("--append-system-prompt-file".into());
            argv.push(file.clone());
        }
        if !spec.tools.is_empty() {
            argv.push("--allowedTools".into());
            argv.push(spec.tools.join(","));
        }
        RunnerCommand {
            argv,
            env: spec.env.clone(),
            workdir: Some(spec.cwd_in_runner.clone()),
            name: name.into(),
            ..Default::default()
        }
    }

    /// `claude setup-token`, under a pty, in a directory of its own.
    ///
    /// The pty is not a nicety. With plain pipes the command prints nothing at
    /// all — it is an Ink UI, and it reads its keystrokes from `/dev/tty`
    /// rather than from stdin — so a login spawned the way a session is
    /// spawned would hang with no URL and no way to answer it. `script(1)`
    /// (bsdutils, part of the image's Debian base) gives it one and forwards
    /// this process's stdin into it, which is what carries the paste-back.
    ///
    /// `HOME` and the state directory are a throwaway path under `/tmp`, never
    /// the state a session mounts: the login is a helper that runs for a
    /// couple of minutes and leaves nothing behind, and its token is read off
    /// its own output rather than out of a store.
    fn setup_token_cmd(&self, name: &str, helper_home: &str) -> RunnerCommand {
        let config_dir = format!("{helper_home}{}", Self::CONFIG_SUBDIR);
        let script = format!(
            "mkdir -p {config_dir} && stty cols {LOGIN_COLUMNS} && exec claude setup-token"
        );
        RunnerCommand {
            argv: vec![
                "script".into(),
                // Quiet, and exit with the command's own status rather than
                // always zero, so a login that failed reads as failed.
                "-q".into(),
                "-e".into(),
                "-c".into(),
                script,
                "/dev/null".into(),
            ],
            env: vec![
                ("HOME".into(), helper_home.into()),
                (Self::layout().env.into(), config_dir),
                // Nothing in the runner can open a browser, and the CLI prints
                // the URL either way; this keeps it from waiting on a spawn
                // that would fail.
                ("BROWSER".into(), "/bin/true".into()),
                ("DISABLE_AUTOUPDATER".into(), "1".into()),
                ("DISABLE_TELEMETRY".into(), "1".into()),
            ],
            image: self.login_image.clone(),
            name: name.into(),
            ..Default::default()
        }
    }

    /// A directory for one login helper, under `/tmp` so that it is gone with
    /// the container and can never be a session's state directory.
    fn helper_home() -> String {
        format!("/tmp/tracon-setup-token-{}", uuid::Uuid::now_v7())
    }

    const CONFIG_SUBDIR: &'static str = "/.claude";
}

/// Everything a terminal wrote to position, colour, or hyperlink the text,
/// removed: CSI sequences, OSC strings (which is how the CLI wraps the sign-in
/// URL in a clickable link), and the odd two-byte escape. What is left is what
/// an operator would have read on the screen.
fn plain(line: &str) -> String {
    let bytes: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != '\u{1b}' {
            if bytes[i] != '\r' && bytes[i] != '\u{7}' {
                out.push(bytes[i]);
            }
            i += 1;
            continue;
        }
        i += 1;
        match bytes.get(i) {
            // CSI: parameters, then one final byte in @..~.
            Some('[') => {
                i += 1;
                while i < bytes.len() && !('\u{40}'..='\u{7e}').contains(&bytes[i]) {
                    i += 1;
                }
                i += 1;
            }
            // OSC: runs to BEL or ST. Its payload (a hyperlink target) is not
            // what the operator read, so it goes with the sequence.
            Some(']') => {
                i += 1;
                while i < bytes.len() && bytes[i] != '\u{7}' {
                    if bytes[i] == '\u{1b}' && bytes.get(i + 1) == Some(&'\\') {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            // Two-byte escapes: charset selection, save/restore cursor, ...
            Some(_) => i += 1,
            None => break,
        }
    }
    out
}

/// The sign-in URL on a line of the login's screen, if it is there whole.
///
/// The authorize page is on Anthropic's own host and its redirect goes to a
/// hosted callback, not to localhost: there is no local listener to catch, so
/// the code comes back to the operator and is pasted in.
fn sign_in_url(line: &str) -> Option<String> {
    plain(line)
        .split_whitespace()
        .find(|token| token.starts_with("https://"))
        .map(|token| token.trim_end_matches(['.', ',']).to_string())
}

/// The minted token on a line of the login's screen.
fn minted_token(line: &str) -> Option<String> {
    plain(line)
        .split_whitespace()
        .find(|token| token.starts_with(TOKEN_PREFIX))
        .map(str::to_string)
}

/// How long the CLI says the token it just printed is good for, in
/// milliseconds. `Your OAuth token (valid for 364 days):` and the like; a
/// line that says nothing about a lifetime gives `None`, and the caller
/// falls back to the one the CLI's own banner promises.
fn stated_lifetime_ms(line: &str) -> Option<i64> {
    let plain = plain(line);
    let rest = plain.split("valid for").nth(1)?;
    let mut words = rest.split_whitespace();
    let count: i64 = words.next()?.parse().ok()?;
    let unit = words.next()?.trim_end_matches([')', ':', ',', '.']);
    let ms = match unit.trim_end_matches('s') {
        "day" => count * MS_PER_DAY,
        "week" => count * 7 * MS_PER_DAY,
        "month" => count * 30 * MS_PER_DAY,
        "year" => count * DEFAULT_TOKEN_DAYS * MS_PER_DAY,
        "hour" => count * 60 * 60 * 1000,
        _ => return None,
    };
    (ms > 0).then_some(ms)
}

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
    // "2.1.247 (Claude Code)"
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

pub struct ClaudeHandle {
    writer: Writer,
    session_id: String,
    turn: Arc<Mutex<Option<oneshot::Sender<TurnResult>>>>,
    compat: HarnessCompat,
}

#[async_trait]
impl HarnessHandle for ClaudeHandle {
    fn harness_session_id(&self) -> &str {
        &self.session_id
    }

    fn compat(&self) -> HarnessCompat {
        self.compat.clone()
    }

    async fn prompt(&self, text: String) -> Result<TurnResult, AdapterError> {
        let (tx, rx) = oneshot::channel();
        *self.turn.lock().unwrap() = Some(tx);
        self.writer
            .send(json!({
                "type": "user",
                "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
            }))
            .await?;
        rx.await
            .map_err(|_| AdapterError::Protocol("the harness ended mid-turn".into()))
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
}

fn text_of(block: &Value) -> String {
    block["text"].as_str().unwrap_or_default().to_string()
}

impl Pump {
    async fn run(self, tx: mpsc::Sender<HarnessEvent>) {
        let Pump {
            stdout,
            done,
            writer,
            turn,
            open,
        } = self;
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match v["type"].as_str().unwrap_or_default() {
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
                                    }))
                                    .await;
                            }
                            _ => {}
                        }
                    }
                }
                // A tool's result comes back as a synthetic user message.
                "user" => {
                    for block in v["message"]["content"].as_array().into_iter().flatten() {
                        if block["type"] == "tool_result" {
                            let _ = tx
                                .send(HarnessEvent::ToolCallUpdate(ToolCallUpdate {
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
                                    raw_output: None,
                                }))
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
                    let (reply_tx, reply_rx) = oneshot::channel();
                    open.lock().unwrap().insert(id.clone(), ());
                    let _ = tx
                        .send(HarnessEvent::Permission {
                            request: PermissionRequest {
                                tool_call_id: v["request"]["tool_use_id"]
                                    .as_str()
                                    .map(str::to_string),
                                title: summarize(&tool, &input),
                                kind: Some("tool".into()),
                                raw_input: Some(input),
                                options: options(),
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
                        Some(_) => "refusal",
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
    Usage {
        input_tokens: input,
        output_tokens: output,
        // The CLI reports no total; `charged()` falls back to the sum, and
        // stating it here keeps the budget from reading zero.
        total_tokens: input + output + cached,
        cached_read_tokens: cached,
    }
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

/// Read frames until `system/init`, which the CLI emits before any model call.
/// Returns the init frame and the reader positioned after it.
type Started = (
    Value,
    Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    Writer,
    futures_core::future::BoxFuture<'static, Result<i32, RunnerError>>,
);

async fn read_init(spawned: Spawned) -> Result<Started, AdapterError> {
    let Spawned {
        stdin,
        stdout,
        done,
        ..
    } = spawned;
    let writer = Writer::spawn(stdin);
    let mut reader = BufReader::new(stdout);
    let deadline = std::time::Duration::from_secs(INIT_TIMEOUT_SECS);
    let init = tokio::time::timeout(deadline, async {
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .await
                .map_err(|e| AdapterError::Protocol(e.to_string()))?;
            if n == 0 {
                return Err(AdapterError::Protocol(
                    "the harness exited before it started a session".into(),
                ));
            }
            if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                if v["type"] == "system" && v["subtype"] == "init" {
                    return Ok(v);
                }
            }
        }
    })
    .await
    .map_err(|_| AdapterError::Protocol("the harness never started a session".into()))??;
    Ok((init, Box::new(reader), writer, done))
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

    async fn version(&self, runner: &dyn Runner) -> Result<HarnessVersion, AdapterError> {
        let out = runner
            .run_capture(RunnerCommand {
                argv: vec!["claude".into(), "--version".into()],
                name: "claude-version".into(),
                ..Default::default()
            })
            .await?;
        Ok(HarnessVersion {
            found: parse_version(&String::from_utf8_lossy(&out.stdout)),
            pinned: self.pinned.clone(),
        })
    }

    /// Claude Code has no model catalogue to ask for, so this reports the
    /// aliases it accepts with the one it would default to first. Launching to
    /// read `system/init` costs a process and no model call.
    async fn probe_models(
        &self,
        runner: &dyn Runner,
        wiring: &crate::gateway::model::Wiring,
    ) -> Result<Vec<ModelOption>, AdapterError> {
        let spec = LaunchSpec {
            cwd_in_runner: "/".into(),
            model: "sonnet".into(),
            container_name: "claude-probe".into(),
            harness_home: String::new(),
            mcp_servers: Vec::new(),
            tools: Vec::new(),
            env: wiring.env.clone(),
            system_prompt_file: None,
            cursor: None,
        };
        let session_id = uuid::Uuid::now_v7().to_string();
        let spawned = runner
            .spawn(Self::cmd("claude-probe", &spec, &session_id))
            .await?;
        let (init, _reader, writer, done) = read_init(spawned).await?;
        let default = init["model"].as_str().unwrap_or_default().to_string();
        // The probe has what it came for; closing stdin ends the run cleanly
        // rather than leaving the kill to a dropped handle.
        drop(writer);
        drop(done);
        let mut out: Vec<ModelOption> = ALIASES
            .iter()
            .map(|a| ModelOption {
                value: (*a).to_string(),
                name: (*a).to_string(),
            })
            .collect();
        if !default.is_empty() && !ALIASES.iter().any(|a| default.starts_with(a)) {
            out.insert(
                0,
                ModelOption {
                    value: default.clone(),
                    name: format!("{default} (this node's default)"),
                },
            );
        }
        Ok(out)
    }

    async fn launch(
        &self,
        runner: &dyn Runner,
        spec: LaunchSpec,
    ) -> Result<(Box<dyn HarnessHandle>, mpsc::Receiver<HarnessEvent>), AdapterError> {
        // The node picks the session id rather than discovering it, so the row
        // it has already written and the harness's own id are the same string
        // even if the handshake fails.
        let session_id = uuid::Uuid::now_v7().to_string();
        let spawned = runner
            .spawn(Self::cmd(&spec.container_name, &spec, &session_id))
            .await?;
        let (init, reader, writer, done) = read_init(spawned).await?;

        // Enforce the pin a second time, from the harness's own report rather
        // than from a `--version` call that may have run against a different
        // image. A missing version is a mismatch, not a pass.
        let found = init["claude_code_version"].as_str().unwrap_or("unknown");
        if found != self.pinned {
            return Err(AdapterError::VersionMismatch {
                found: found.to_string(),
                pinned: self.pinned.clone(),
            });
        }
        // Everything below decodes stream-json frames of one revision. A CLI
        // that starts naming another one is refused rather than driven.
        let protocol = init_protocol(&init);
        if !Self::PROTOCOL.accepts(protocol) {
            return Err(AdapterError::IncompatibleProtocol {
                agent: Self::ID.into(),
                protocol: Self::PROTOCOL.name,
                found: protocol.to_string(),
                supported: Self::PROTOCOL.versions(),
            });
        }
        // A server the harness could not reach means the session has no tools
        // and would fail in a way that looks like the model being unhelpful.
        for s in init["mcp_servers"].as_array().into_iter().flatten() {
            if s["status"].as_str().is_some_and(|st| st != "connected") {
                return Err(AdapterError::Protocol(format!(
                    "the harness could not connect to the {} MCP server ({})",
                    s["name"].as_str().unwrap_or("tracon"),
                    s["status"].as_str().unwrap_or("unknown")
                )));
            }
        }

        let turn = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel(256);
        let handle = ClaudeHandle {
            writer: writer.clone(),
            session_id: init["session_id"]
                .as_str()
                .unwrap_or(&session_id)
                .to_string(),
            turn: turn.clone(),
            compat: HarnessCompat {
                agent: Self::ID.into(),
                version: found.to_string(),
                protocol: Self::PROTOCOL.tag(protocol),
            },
        };
        tokio::spawn(
            Pump {
                stdout: reader,
                done,
                writer,
                turn,
                open: Arc::new(Mutex::new(HashMap::new())),
            }
            .run(tx),
        );
        Ok((Box::new(handle), rx))
    }

    /// `claude setup-token`: the operator signs in in their own browser and
    /// pastes the code back, and the CLI prints a long-lived subscription
    /// token.
    ///
    /// The redirect goes to a hosted callback on Anthropic's own site rather
    /// than to localhost, so there is no local listener to intercept and no
    /// device code to show: the paste-back is the whole completion. The token
    /// is read off the CLI's own output, because it is printed once and
    /// stored nowhere a `lift` could go looking for it.
    /// `state_dir` is unused: `claude setup-token` prints its token and
    /// writes nothing, so there is no store to steer (see `lift`).
    async fn login(
        &self,
        runner: &dyn Runner,
        provider: &str,
        name: &str,
        _state_dir: &str,
    ) -> Result<LoginFlow, AdapterError> {
        if provider != Self::LOGIN_PROVIDER {
            return Err(AdapterError::Protocol(format!(
                "Claude Code signs in to an Anthropic subscription, not to {provider}"
            )));
        }
        let helper_home = Self::helper_home();
        let spawned = runner
            .spawn(self.setup_token_cmd(name, &helper_home))
            .await?;
        let Spawned {
            mut stdin,
            stdout,
            done,
            endpoint: _,
        } = spawned;
        let output = Arc::new(Mutex::new(String::new()));
        let mut lines = BufReader::new(stdout).lines();

        let url = tokio::time::timeout(LOGIN_URL_TIMEOUT, async {
            while let Ok(Some(line)) = lines.next_line().await {
                record(&output, &line);
                if let Some(url) = sign_in_url(&line) {
                    return Some(url);
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
        .ok_or_else(|| {
            AdapterError::Protocol("`claude setup-token` printed no sign-in URL".into())
        })?;

        // Two listeners for the one event: the writer, which answers the
        // CLI's last screen, and the exit future, which stops waiting on it.
        let (captured_for_writer, writer_signal) = oneshot::channel::<()>();
        let (captured_for_exit, exit_signal) = oneshot::channel::<()>();

        let minted = self.minted.clone();
        let provider_key = provider.to_string();
        let drained = output.clone();
        tokio::spawn(async move {
            let mut signals = vec![captured_for_writer, captured_for_exit];
            let mut lifetime_ms = None;
            while let Ok(Some(line)) = lines.next_line().await {
                record(&drained, &line);
                if lifetime_ms.is_none() {
                    // Printed on the line above the token, so it is known by
                    // the time the token itself is read.
                    lifetime_ms = stated_lifetime_ms(&line);
                }
                if signals.is_empty() {
                    continue;
                }
                let Some(access) = minted_token(&line) else {
                    continue;
                };
                minted.lock().unwrap().insert(
                    provider_key.clone(),
                    LiftedToken {
                        access,
                        refresh: None,
                        expires_ms: Some(
                            crate::store::now_ms()
                                + lifetime_ms.unwrap_or(DEFAULT_TOKEN_DAYS * MS_PER_DAY),
                        ),
                        // `setup-token` names no account: it prints a token and
                        // nothing else, and the helper's home is thrown away
                        // with the container, so there is nothing to ask.
                        identity: None,
                        account_id: None,
                    },
                );
                for signal in signals.drain(..) {
                    let _ = signal.send(());
                }
            }
        });

        // The node's side of the paste-back. It goes through a pipe of the
        // adapter's own rather than straight to the process, because what the
        // node writes is a line ending in LF and what a pty in raw mode
        // delivers as Enter is CR — the CLI's input would take an LF as one
        // more character of the code and never submit it.
        let (client, mut server) = tokio::io::duplex(16 * 1024);
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let mut captured = writer_signal;
            let mut armed = true;
            loop {
                tokio::select! {
                    read = server.read(&mut buf) => {
                        let Ok(n) = read else { break };
                        if n == 0 {
                            break;
                        }
                        if forward_typed(&mut stdin, &buf[..n]).await.is_err() {
                            break;
                        }
                    }
                    signal = &mut captured, if armed => {
                        armed = false;
                        if signal.is_ok() {
                            // The token has been read off the screen. The CLI's
                            // last frame waits on a keypress that is never
                            // coming inside a container; this is it.
                            let _ = stdin.write_all(b"\r").await;
                            let _ = stdin.flush().await;
                            break;
                        }
                    }
                }
            }
        });

        let done: futures_core::future::BoxFuture<'static, Result<i32, RunnerError>> =
            Box::pin(async move {
                let mut exit = done;
                tokio::select! {
                    status = &mut exit => status,
                    signal = exit_signal => {
                        if signal.is_err() {
                            return (&mut exit).await;
                        }
                        // The token is what the login was for. Give the CLI a
                        // moment to end on its own so the container is reaped,
                        // then call it done however its last screen behaves.
                        let _ = tokio::time::timeout(LOGIN_EXIT_GRACE, &mut exit).await;
                        Ok(0)
                    }
                }
            });

        Ok(LoginFlow {
            url,
            device_code: None,
            stdin: Box::new(client),
            done,
            output,
        })
    }

    /// There is nothing to refresh. A `setup-token` token is long-lived and
    /// has no refresh token behind it: when it runs out, the only way to get
    /// another is to sign in again. Saying so as its own error is what keeps
    /// the refresh loop from asking every five minutes for the rest of the
    /// node's life.
    async fn refresh(
        &self,
        _runner: &dyn Runner,
        provider: &str,
        _name: &str,
    ) -> Result<(), AdapterError> {
        Err(AdapterError::ReconnectRequired(format!(
            "the {provider} subscription token was minted by `claude setup-token` and cannot be \
             renewed in place; connect the provider again to mint a new one"
        )))
    }

    /// What the login printed. There is no store to read: see `minted`.
    async fn lift(&self, _store_dir: &Path, provider: &str) -> Result<LiftedToken, AdapterError> {
        self.minted.lock().unwrap().remove(provider).ok_or_else(|| {
            AdapterError::Protocol(format!(
                "`claude setup-token` printed no {provider} token; sign in again"
            ))
        })
    }
}

/// Write what the node sent to the login's pty, each line's text and its Enter
/// as separate writes.
///
/// The CLI's input reads a multi-byte chunk as a paste, and a CR inside a
/// paste is taken as text rather than as Enter: code and CR in one write sit
/// in the prompt and are never submitted. An LF is also only ever one more
/// character to a pty in raw mode, so Enter is sent as CR.
async fn forward_typed<W: AsyncWrite + Unpin>(stdin: &mut W, chunk: &[u8]) -> std::io::Result<()> {
    let mut lines = chunk.split(|byte| *byte == b'\n').peekable();
    while let Some(text) = lines.next() {
        if !text.is_empty() {
            stdin.write_all(text).await?;
            stdin.flush().await?;
        }
        if lines.peek().is_some() {
            tokio::time::sleep(PASTE_SETTLE).await;
            stdin.write_all(b"\r").await?;
            stdin.flush().await?;
        }
    }
    Ok(())
}

/// Keep what a login said, for the reason when it fails, bounded so a chatty
/// UI cannot grow it without limit.
fn record(output: &Arc<Mutex<String>>, line: &str) {
    let mut held = output.lock().unwrap();
    held.push_str(&plain(line));
    held.push('\n');
    if held.len() > 64 * 1024 {
        // On a character boundary: the login's screen is not ASCII, and
        // `drain` panics in the middle of one.
        let cut = (held.len() - 32 * 1024..held.len())
            .find(|at| held.is_char_boundary(*at))
            .unwrap_or(0);
        held.drain(..cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> LaunchSpec {
        LaunchSpec {
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
            cursor: None,
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
        assert!(!argv.contains("bypassPermissions"), "{argv}");
        assert!(!argv.contains("dontAsk"), "{argv}");
        assert!(argv.contains("--strict-mcp-config"), "{argv}");
        assert!(argv.contains("--input-format stream-json"), "{argv}");
        assert!(argv.contains("--output-format stream-json"), "{argv}");
        assert!(argv.contains("--session-id sid"), "{argv}");
        assert!(argv.contains("--model opus"), "{argv}");
        assert!(
            argv.contains("--append-system-prompt-file /root/.claude/orientation.md"),
            "{argv}"
        );
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
    }

    #[test]
    fn a_version_is_read_out_of_the_cli_banner() {
        assert_eq!(parse_version("2.1.247 (Claude Code)\n"), "2.1.247");
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
            "usage": { "input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 5 }
        }));
        assert_eq!(u.charged(), 125);
    }

    #[test]
    fn the_state_directory_is_this_harnesss_own() {
        let l = ClaudeAdapter::layout();
        assert_eq!(l.dir, ".claude");
        assert_eq!(l.env, "CLAUDE_CONFIG_DIR");
    }

    /// The login's screen is a terminal UI, so the URL and the token arrive
    /// inside colour, cursor moves and an OSC 8 hyperlink. What the node reads
    /// has to be what the operator would have read.
    #[test]
    fn the_screen_is_read_the_way_an_operator_would_read_it() {
        let url = "https://claude.com/cai/oauth/authorize?code=true&state=abc";
        let line = format!(
            "\u{1b}]8;id=1t7is4t;{url}\u{7}\u{1b}[38;2;153;153;153m{url}\u{1b}[39m\u{1b}]8;;\u{7}\r"
        );
        assert_eq!(plain(&line), url);
        assert_eq!(sign_in_url(&line).as_deref(), Some(url));
        // The hyperlink target is not text the operator read, so a line whose
        // only URL is inside one yields nothing.
        assert_eq!(
            sign_in_url("\u{1b}]8;;https://elsewhere\u{7}\u{1b}]8;;\u{7}"),
            None
        );
        assert_eq!(sign_in_url("\u{1b}[2GPaste code here if prompted > "), None);
    }

    #[test]
    fn the_minted_token_is_picked_out_of_its_own_frame() {
        assert_eq!(
            minted_token("\u{1b}[1msk-ant-oat01-abc123\u{1b}[22m\r").as_deref(),
            Some("sk-ant-oat01-abc123")
        );
        // An API key is not a subscription token and must not be mistaken for
        // one: only the long-lived prefix counts.
        assert_eq!(minted_token("sk-ant-api03-abc123"), None);
        assert_eq!(minted_token("Store this token securely."), None);
    }

    #[test]
    fn a_stated_lifetime_is_preferred_to_the_assumed_one() {
        assert_eq!(
            stated_lifetime_ms("Your OAuth token (valid for 364 days):"),
            Some(364 * MS_PER_DAY)
        );
        assert_eq!(
            stated_lifetime_ms("Your OAuth token (valid for 1 year):"),
            Some(365 * MS_PER_DAY)
        );
        // "less than a day" is the CLI's own fallback wording; nothing to
        // derive from it, so the caller's assumed horizon applies.
        assert_eq!(stated_lifetime_ms("valid for less than a day"), None);
        assert_eq!(stated_lifetime_ms("Store this token securely."), None);
    }

    /// The login runs in a directory of its own, under /tmp, so it can never
    /// be the state a session mounts — and `script(1)` is what gives it the
    /// pty without which the CLI prints nothing at all.
    #[test]
    fn the_login_command_runs_under_a_pty_in_its_own_home() {
        let adapter = ClaudeAdapter::new("2.1.247").with_login_image(Some("localhost/c".into()));
        let home = ClaudeAdapter::helper_home();
        let cmd = adapter.setup_token_cmd("tracon-login-anthropic-1", &home);
        assert_eq!(cmd.argv[0], "script");
        assert!(cmd.argv.contains(&"-e".to_string()), "{:?}", cmd.argv);
        let script = cmd.argv.iter().find(|a| a.contains("claude")).unwrap();
        assert!(script.contains("stty cols 400"), "{script}");
        assert!(script.ends_with("exec claude setup-token"), "{script}");
        assert!(home.starts_with("/tmp/tracon-setup-token-"), "{home}");
        assert_eq!(cmd.image.as_deref(), Some("localhost/c"));
        assert!(cmd.mounts.is_empty());
    }
}
