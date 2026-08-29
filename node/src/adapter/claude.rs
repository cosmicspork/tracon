//! The Claude Code adapter. Drives `claude` headless over stream-json on the
//! runner's stdio.
//!
//! Claude Code does not speak ACP, and `ARCHITECTURE.md:141` already said so:
//! its `control_request` / `control_response` path is functionally equivalent
//! to ACP permission requests but is not ACP. That path is what this adapter
//! is built on. It is the same protocol the published Agent SDKs speak — they
//! spawn this same binary and talk stream-json to it — so there is no reason
//! to take a Node dependency to reach it from Rust.
//!
//! The shapes below were read out of the shipped 2.1.247 binary and confirmed
//! against a live run, because the CLI's `--help` documents neither the
//! control protocol nor `--permission-prompt-tool`. `phase-7-notes.md` records
//! them, since the next person will not find them in the published docs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::{
    AdapterError, HarnessAdapter, HarnessEvent, HarnessHandle, HarnessVersion, LaunchSpec, Layout,
    ModelOption, PermissionReply, PermissionRequest, TurnResult,
};
use crate::acp::types::{self, PermissionOption, ToolCall, ToolCallUpdate, Usage};
use crate::runner::{Runner, RunnerCommand, RunnerError, Spawned};

/// How long to wait for the `system/init` frame before giving up on a launch.
/// It is emitted before any model call, so this only has to cover process
/// start inside a container.
const INIT_TIMEOUT_SECS: u64 = 60;

/// The models this adapter offers. Claude Code has no catalogue endpoint of
/// its own; these are the aliases it resolves to whatever is current, which is
/// also what keeps a pinned node from silently following a model change.
const ALIASES: &[&str] = &["opus", "sonnet", "haiku"];

pub struct ClaudeAdapter {
    pinned: String,
}

impl ClaudeAdapter {
    pub const ID: &'static str = "claude";

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
}

#[async_trait]
impl HarnessHandle for ClaudeHandle {
    fn harness_session_id(&self) -> &str {
        &self.session_id
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
        PermissionReply::Selected(_) => json!({
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
        env: Vec<(String, String)>,
    ) -> Result<Vec<ModelOption>, AdapterError> {
        let spec = LaunchSpec {
            cwd_in_runner: "/".into(),
            model: "sonnet".into(),
            container_name: "claude-probe".into(),
            mcp_servers: Vec::new(),
            tools: Vec::new(),
            env,
            system_prompt_file: None,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> LaunchSpec {
        LaunchSpec {
            cwd_in_runner: "/work".into(),
            model: "opus".into(),
            container_name: "c".into(),
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
}
