//! Protocol mode: the node drives `tracon-agent` over its stdin and stdout
//! with the shared ACP vocabulary (`acp::types`), as it once drove omp.
//!
//! Every tool call, built-in or node, is asked through
//! `session/request_permission` before it runs; what may run unasked is the
//! node's policy, not the harness's. Each `session/update` carries a `seq` and
//! is mirrored to the events file, so a session's record survives the pipe.
//!
//! With a journal directory, each session also writes its journal
//! (`journal.rs`), and `session/load` carries a session on in a new process:
//! the node starts one when the last died, and the turn it died in is closed
//! from the journal.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acp::codec::RpcError;
use acp::rpc::{Incoming, Peer, Reader, Writer};
use acp::types::{
    self, methods, AgentInfo, Chunk, ContentBlock, InitializeResult, LoadSessionParams,
    NewSessionParams, NewSessionResult, PermissionOption, PermissionOutcome, PromptParams,
    PromptResult, RequestPermissionParams, RequestPermissionResult, RequestUsage, SessionIdParams,
    SessionUpdate, SessionUpdateParams, ToolCall, ToolCallUpdate, UsageUpdate,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, Compaction, Hooks, Outcome, Permit, Totals};
use crate::conversation::{Block, Conversation, Role};
use crate::events::{self, Events};
use crate::journal::{self, Journal, Record, Restored};
use crate::prompt::ORIENTATION;
use crate::subagent::{self, Spawner};
use crate::tools::{mcp, Context, Output, Registry};
use crate::wire::{Client, Delta};
use crate::wire::{Usage, WireError};

/// Repository instruction files read into the system prompt, in this order.
const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];
/// Past this, an instruction file is cut: a small context window has to hold
/// the work as well.
const MAX_INSTRUCTIONS: usize = 32 * 1024;

pub struct Settings {
    pub client: Client,
    /// Model requests one prompt may make; `None` is no limit.
    pub max_turns: Option<u32>,
    /// Appended to the orientation: the node's operator notes and
    /// customization, from `--system-prompt-file`.
    pub system_extra: Option<String>,
    pub context_window: Option<u64>,
    pub events: Option<PathBuf>,
    /// Where each session's journal is kept. Without one nothing can be
    /// carried on, and `session/load` is not offered.
    pub journal_dir: Option<PathBuf>,
    /// Which models a subagent may be given; `None` turns `spawn` off.
    pub spawner: Option<Spawner>,
}

/// Serves one node connection until it closes stdin.
pub async fn serve(settings: Settings, reader: Reader, writer: Writer) -> anyhow::Result<()> {
    let settings = Arc::new(settings);
    let events = Arc::new(match &settings.events {
        Some(path) => Events::to_file(path)?,
        None => Events::to_stderr(),
    });
    let (peer, mut incoming, read) = Peer::new(writer, reader);
    let reading = tokio::spawn(read);
    let mut sessions: HashMap<String, Arc<Session>> = HashMap::new();
    while let Some(message) = incoming.recv().await {
        match message {
            Incoming::Request {
                method,
                params,
                reply,
                ..
            } => match method.as_str() {
                methods::INITIALIZE => {
                    let _ = reply.send(Ok(json!(InitializeResult {
                        protocol_version: types::PROTOCOL_MAX,
                        agent_info: Some(AgentInfo {
                            name: "tracon-agent".into(),
                            version: env!("CARGO_PKG_VERSION").into(),
                            extra: Default::default(),
                        }),
                        extra: [(
                            "agentCapabilities".to_string(),
                            json!({"loadSession": settings.journal_dir.is_some()}),
                        )]
                        .into_iter()
                        .collect(),
                    })));
                }
                methods::SESSION_NEW => {
                    let result = match serde_json::from_value::<NewSessionParams>(params) {
                        Ok(p) => {
                            let id = uuid::Uuid::now_v7().to_string();
                            let opened = Session::start(
                                &id,
                                &p.cwd,
                                &p.mcp_servers,
                                None,
                                &settings,
                                &peer,
                                &events,
                            )
                            .await;
                            match opened {
                                Ok(s) => {
                                    sessions.insert(id.clone(), Arc::new(s));
                                    Ok(json!(NewSessionResult {
                                        session_id: id,
                                        config_options: Vec::new(),
                                        extra: Default::default(),
                                    }))
                                }
                                Err(e) => Err(invalid(e)),
                            }
                        }
                        Err(e) => Err(invalid(e.to_string())),
                    };
                    let _ = reply.send(result);
                }
                methods::SESSION_LOAD => {
                    let result = match serde_json::from_value::<LoadSessionParams>(params) {
                        Ok(p) if sessions.contains_key(&p.session_id) => {
                            Err(invalid(format!("session {} is already open", p.session_id)))
                        }
                        Ok(p) => match load(&p, &settings, &peer, &events).await {
                            Ok(s) => {
                                sessions.insert(p.session_id, Arc::new(s));
                                Ok(json!({}))
                            }
                            Err(e) => Err(invalid(e)),
                        },
                        Err(e) => Err(invalid(e.to_string())),
                    };
                    let _ = reply.send(result);
                }
                methods::SESSION_PROMPT => {
                    let p = match serde_json::from_value::<PromptParams>(params) {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = reply.send(Err(invalid(e.to_string())));
                            continue;
                        }
                    };
                    let Some(session) = sessions.get(&p.session_id).cloned() else {
                        let _ = reply.send(Err(invalid(format!("no session {}", p.session_id))));
                        continue;
                    };
                    let settings = settings.clone();
                    // The read loop keeps serving cancel and permission
                    // answers while the turn runs.
                    tokio::spawn(async move {
                        let _ = reply.send(session.prompt(p.prompt, &settings).await);
                    });
                }
                methods::SESSION_CLOSE => {
                    if let Ok(p) = serde_json::from_value::<SessionIdParams>(params) {
                        if let Some(s) = sessions.remove(&p.session_id) {
                            s.cancel();
                        }
                    }
                    let _ = reply.send(Ok(Value::Null));
                }
                other => {
                    let _ = reply.send(Err(RpcError::method_not_found(other)));
                }
            },
            Incoming::Notification { method, params } => {
                if method == methods::SESSION_CANCEL {
                    if let Some(s) = serde_json::from_value::<SessionIdParams>(params)
                        .ok()
                        .and_then(|p| sessions.get(&p.session_id))
                    {
                        s.cancel();
                    }
                }
            }
        }
    }
    for s in sessions.values() {
        s.cancel();
    }
    reading.abort();
    Ok(())
}

/// Reopens a journaled session in this process, closing the turn the last one
/// died in.
async fn load(
    p: &LoadSessionParams,
    settings: &Settings,
    peer: &Peer,
    events: &Arc<Events>,
) -> Result<Session, String> {
    let Some(dir) = &settings.journal_dir else {
        return Err("this agent keeps no journal, so it cannot load a session".into());
    };
    let path = Journal::path(dir, &p.session_id);
    let restored = journal::restore(&path)
        .map_err(|e| format!("no journal for session {}: {e}", p.session_id))?;
    Session::start(
        &p.session_id,
        &p.cwd,
        &p.mcp_servers,
        Some(restored),
        settings,
        peer,
        events,
    )
    .await
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError {
        code: -32602,
        message: message.into(),
        data: None,
    }
}

struct Session {
    id: String,
    cx: Context,
    tools: Registry,
    events: Arc<Events>,
    journal: Option<Journal>,
    out: mpsc::UnboundedSender<Out>,
    /// One prompt at a time; the next waits for the conversation.
    state: tokio::sync::Mutex<(Conversation, Totals)>,
    turn: Mutex<Option<CancellationToken>>,
}

/// What a session sends the node, in order.
enum Out {
    Update(SessionUpdate),
    Permission(RequestPermissionParams, oneshot::Sender<Permit>),
    /// Answered once everything queued before it has been sent.
    Flush(oneshot::Sender<()>),
}

impl Session {
    /// A new session, or with `restored` one carried on from its journal.
    async fn start(
        id: &str,
        cwd: &str,
        mcp_servers: &[Value],
        restored: Option<Restored>,
        settings: &Settings,
        peer: &Peer,
        events: &Arc<Events>,
    ) -> Result<Self, String> {
        let cx = Context::new(std::path::Path::new(cwd)).map_err(|e| format!("cwd {cwd}: {e}"))?;
        let mut tools = Registry::default();
        for config in mcp_servers {
            let server = mcp::Server::from_config(config)?;
            for tool in server.connect().await? {
                tools.add(tool);
            }
        }
        // Built afresh on a load too: the orientation and instruction files
        // are today's, while the conversation is the journal's.
        let (system, read) = system_prompt(&cx, settings.system_extra.as_deref());
        let journal = match &settings.journal_dir {
            Some(dir) => {
                Some(Journal::open(dir, id).map_err(|e| format!("journal for session {id}: {e}"))?)
            }
            None => None,
        };
        let loaded = restored.is_some();
        let Restored {
            messages,
            interrupted,
            ..
        } = restored.unwrap_or_default();
        if let (Some(j), false) = (&journal, loaded) {
            j.write(&Record::Session {
                id: id.to_string(),
                cwd: cwd.to_string(),
            });
        }
        events.emit(
            "session",
            json!({
                "session_id": id,
                "cwd": cx.root,
                "tools": tools.names(),
                "instructions_read": read,
                "loaded": loaded.then_some(json!({
                    "messages": messages.len(),
                    "interrupted": interrupted.iter().map(|i| &i.id).collect::<Vec<_>>(),
                })),
            }),
        );
        let first_seq = match (&settings.events, loaded) {
            (Some(path), true) => events::last_seq(path, id),
            _ => 0,
        };
        let (out, rx) = mpsc::unbounded_channel();
        tokio::spawn(send_in_order(
            id.to_string(),
            first_seq,
            rx,
            peer.clone(),
            events.clone(),
        ));
        // The node is told how each call the last process left open ends, as
        // it would have been had that process lived to say so.
        for call in &interrupted {
            if let Some(j) = &journal {
                j.write(&Record::ToolResult {
                    id: call.id.clone(),
                    content: call.content.into(),
                    is_error: true,
                });
            }
            if !call.asked {
                let _ = out.send(Out::Update(SessionUpdate::ToolCall(describe(
                    &call.id,
                    &call.name,
                    &call.input,
                    "pending",
                ))));
            }
            let _ = out.send(Out::Update(SessionUpdate::ToolCallUpdate(result_update(
                &call.id,
                &Output::error(call.content),
            ))));
        }
        let mut conversation = Conversation::new(system);
        conversation.messages = messages;
        conversation.cache_key = Some(id.to_string());
        Ok(Self {
            id: id.to_string(),
            cx,
            tools,
            events: events.clone(),
            journal,
            out,
            state: tokio::sync::Mutex::new((conversation, Totals::default())),
            turn: Mutex::new(None),
        })
    }

    fn cancel(&self) {
        if let Some(t) = self.turn.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            t.cancel();
        }
    }

    async fn prompt(
        &self,
        prompt: Vec<ContentBlock>,
        settings: &Settings,
    ) -> Result<Value, RpcError> {
        let mut state = self.state.lock().await;
        let (conv, totals) = &mut *state;
        let text = prompt
            .into_iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text),
                ContentBlock::Other => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let blocks = vec![Block::Text(text)];
        if let Some(j) = &self.journal {
            j.write(&Record::Message {
                role: Role::User,
                blocks: blocks.clone(),
            });
        }
        conv.push(Role::User, blocks);
        let cancel = CancellationToken::new();
        *self.turn.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel.clone());
        let before = totals.usage.clone();
        let hooks = Wired {
            session: self,
            context_window: settings.context_window,
            cancel: cancel.clone(),
            pending: Mutex::new((false, String::new())),
        };
        let agent = Agent {
            client: &settings.client,
            tools: &self.tools,
            cx: &self.cx,
            events: &self.events,
            hooks: &hooks,
            cancel: &cancel,
            journal: self.journal.as_ref(),
            max_turns: settings.max_turns,
            context_window: settings.context_window,
            spawner: settings.spawner.as_ref(),
            finish: None,
            subagent: None,
        };
        let outcome = agent.run(conv, totals).await;
        hooks.flush_text();
        *self.turn.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // The node reads the turn's result as its end: every update the turn
        // made goes out first.
        let (flushed, wait) = oneshot::channel();
        if self.out.send(Out::Flush(flushed)).is_ok() {
            let _ = wait.await;
        }
        let usage = turn_usage(&before, &totals.usage);
        let stop_reason = match outcome {
            // Only a subagent returns; a session's own turn never does.
            Ok(Outcome::Finished { .. } | Outcome::Returned { .. }) => "end_turn",
            Ok(Outcome::MaxTurns) => "max_turn_requests",
            Ok(Outcome::Cancelled) => "cancelled",
            Ok(Outcome::Stopped { reason }) => {
                return Err(RpcError {
                    code: -32000,
                    message: reason,
                    data: None,
                })
            }
            Ok(Outcome::ContextOverflow { message }) => {
                return Err(turn_error(WireError::ContextOverflow(message)))
            }
            Err(e) => return Err(turn_error(e)),
        };
        Ok(json!(PromptResult {
            stop_reason: stop_reason.into(),
            usage: Some(usage),
            extra: Default::default(),
        }))
    }
}

fn turn_error(e: WireError) -> RpcError {
    RpcError {
        code: -32000,
        message: e.to_string(),
        data: None,
    }
}

/// The turn's usage in the node's terms: `input` is the prompt not read from a
/// cache, as the gateway counts it, so the two reconcile.
fn turn_usage(before: &Usage, after: &Usage) -> types::Usage {
    let input = after.input - before.input;
    let output = after.output - before.output;
    let cache_read = (after.cache_read - before.cache_read).min(input);
    types::Usage {
        input_tokens: input - cache_read,
        output_tokens: output,
        total_tokens: input + output,
        cached_read_tokens: cache_read,
        cache_write_tokens: 0,
    }
}

/// The orientation, the node's text, then the repository's instruction files.
/// Returns the prompt and which files were read, so the session can show it.
fn system_prompt(cx: &Context, extra: Option<&str>) -> (String, Vec<String>) {
    let mut system = ORIENTATION.to_string();
    if let Some(extra) = extra.filter(|e| !e.trim().is_empty()) {
        system.push_str("\n\n");
        system.push_str(extra.trim());
    }
    let mut read = Vec::new();
    for name in INSTRUCTION_FILES {
        let Ok(text) = std::fs::read_to_string(cx.root.join(name)) else {
            continue;
        };
        let text: String = text.chars().take(MAX_INSTRUCTIONS).collect();
        system.push_str(&format!(
            "\n\n# {name} (from the repository)\n\n{}",
            text.trim()
        ));
        read.push(name.to_string());
    }
    (system, read)
}

/// Numbers each update, records it, and sends it; permission requests go out
/// through the same queue so they never overtake the tool call they ask about.
async fn send_in_order(
    session_id: String,
    mut seq: u64,
    mut rx: mpsc::UnboundedReceiver<Out>,
    peer: Peer,
    events: Arc<Events>,
) {
    while let Some(out) = rx.recv().await {
        match out {
            Out::Update(update) => {
                seq += 1;
                let params = SessionUpdateParams {
                    session_id: session_id.clone(),
                    update,
                    seq: Some(seq),
                };
                events.emit(
                    "update",
                    json!({"session_id": session_id, "seq": seq, "update": params.update}),
                );
                if peer.notify(methods::SESSION_UPDATE, &params).await.is_err() {
                    break;
                }
            }
            Out::Flush(done) => {
                let _ = done.send(());
            }
            Out::Permission(params, answer) => {
                let peer = peer.clone();
                tokio::spawn(async move {
                    let result: Result<RequestPermissionResult, _> = peer
                        .request(methods::SESSION_REQUEST_PERMISSION, &params)
                        .await;
                    let _ = answer.send(match result {
                        Ok(RequestPermissionResult {
                            outcome: PermissionOutcome::Selected { option_id },
                            ..
                        }) if option_id == types::OPTION_ALLOW_ONCE => Permit::Allow,
                        Ok(RequestPermissionResult {
                            outcome: PermissionOutcome::Selected { .. },
                            message,
                        }) => Permit::Deny(
                            message.unwrap_or_else(|| "The operator declined this call.".into()),
                        ),
                        Ok(RequestPermissionResult {
                            outcome: PermissionOutcome::Cancelled,
                            ..
                        }) => Permit::Cancelled,
                        // An unanswerable ask is a refusal: deny by default.
                        Err(e) => Permit::Deny(format!("permission could not be asked: {e}")),
                    });
                });
            }
        }
    }
}

/// Streamed text is held until this much has gathered, or a line ends:
/// one update per token made a short turn thousands of updates.
const CHUNK: usize = 120;

/// The loop's hooks, wired to the node.
struct Wired<'a> {
    session: &'a Session,
    context_window: Option<u64>,
    cancel: CancellationToken,
    /// Text not yet sent: whether it is thought, and the text.
    pending: Mutex<(bool, String)>,
}

impl Wired<'_> {
    fn send(&self, update: SessionUpdate) {
        self.flush_text();
        let _ = self.session.out.send(Out::Update(update));
    }

    fn flush_text(&self) {
        let (thought, text) = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if pending.1.is_empty() {
                return;
            }
            (pending.0, std::mem::take(&mut pending.1))
        };
        let chunk = Chunk {
            content: ContentBlock::Text { text },
            message_id: None,
        };
        let _ = self.session.out.send(Out::Update(if thought {
            SessionUpdate::AgentThoughtChunk(chunk)
        } else {
            SessionUpdate::AgentMessageChunk(chunk)
        }));
    }

    /// Asks the node whether a call may run, as `tool_call` shows it.
    async fn ask(&self, id: &str, name: &str, tool_call: ToolCall) -> Permit {
        let (answer, wait) = oneshot::channel();
        let params = RequestPermissionParams {
            session_id: self.session.id.clone(),
            tool: Some(name.to_string()),
            tool_call,
            options: vec![
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
            ],
        };
        self.flush_text();
        if self
            .session
            .out
            .send(Out::Permission(params, answer))
            .is_err()
        {
            return Permit::Deny("the node is gone".into());
        }
        let permit = tokio::select! {
            p = wait => p.unwrap_or(Permit::Deny("the permission request was dropped".into())),
            _ = self.cancel.cancelled() => Permit::Cancelled,
        };
        if permit == Permit::Allow {
            self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: id.into(),
                status: Some("in_progress".into()),
                raw_output: None,
                content: Vec::new(),
                title: None,
                kind: None,
            }));
        }
        permit
    }

    fn text(&self, thought: bool, text: &str) {
        let switched = {
            let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.0 != thought && !pending.1.is_empty()
        };
        if switched {
            self.flush_text();
        }
        let full = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.0 = thought;
            pending.1.push_str(text);
            pending.1.len() >= CHUNK || text.contains('\n')
        };
        if full {
            self.flush_text();
        }
    }
}

#[async_trait]
impl Hooks for Wired<'_> {
    fn delta(&self, delta: &Delta<'_>) {
        match delta {
            Delta::Text(t) => self.text(false, t),
            Delta::Reasoning(t) => self.text(true, t),
            Delta::ToolCall(_) => {}
        }
    }

    fn tool_call(&self, id: &str, name: &str, input: &Value) {
        self.send(SessionUpdate::ToolCall(describe(
            id, name, input, "pending",
        )));
    }

    async fn permit(&self, id: &str, name: &str, input: &Value) -> Permit {
        self.ask(id, name, describe(id, name, input, "pending"))
            .await
    }

    fn child_tool_call(&self, parent: &str, label: &str, id: &str, name: &str, input: &Value) {
        self.send(SessionUpdate::ToolCall(child_call(
            parent, label, id, name, input,
        )));
    }

    async fn child_permit(
        &self,
        parent: &str,
        label: &str,
        id: &str,
        name: &str,
        input: &Value,
    ) -> Permit {
        self.ask(id, name, child_call(parent, label, id, name, input))
            .await
    }

    /// Counted by its id like any request; `used` stays the parent's.
    fn child_usage(&self, usage: &Usage, request_id: Option<&str>) {
        let cache_read = usage.cache_read.min(usage.input);
        self.send(SessionUpdate::UsageUpdate(UsageUpdate {
            size: None,
            used: None,
            cost: None,
            request: Some(RequestUsage {
                request_id: request_id.map(str::to_string),
                input_tokens: usage.input - cache_read,
                output_tokens: usage.output,
                cached_read_tokens: cache_read,
            }),
        }));
    }

    fn tool_result(&self, id: &str, _name: &str, output: &Output) {
        self.send(SessionUpdate::ToolCallUpdate(result_update(id, output)));
    }

    /// Shown while the call runs, never recorded as its result: the node
    /// reads a running call's `raw_output` as the tail to show.
    fn tool_progress(&self, id: &str, tail: &str) {
        self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
            tool_call_id: id.into(),
            status: Some("in_progress".into()),
            raw_output: Some(Value::String(tail.to_string())),
            content: Vec::new(),
            title: None,
            kind: None,
        }));
    }

    fn usage(&self, usage: &Usage, request_id: Option<&str>, context_used: u64) {
        let cache_read = usage.cache_read.min(usage.input);
        self.send(SessionUpdate::UsageUpdate(UsageUpdate {
            size: self.context_window,
            used: Some(context_used),
            cost: None,
            request: Some(RequestUsage {
                request_id: request_id.map(str::to_string),
                input_tokens: usage.input - cache_read,
                output_tokens: usage.output,
                cached_read_tokens: cache_read,
            }),
        }));
    }

    /// Said the way the node already reads a harness's retry notice.
    fn retrying(&self, attempt: u32, wait: Duration, error: &WireError) {
        let status = match error {
            WireError::Http { status, .. } => Some(*status),
            _ => None,
        };
        self.send(SessionUpdate::Other(json!({
            "sessionUpdate": "agent_retry",
            "subtype": "api_retry",
            "attempt": attempt,
            "max_retries": MAX_RETRIES,
            "status": status,
            "message": error.to_string(),
            "wait_ms": wait.as_millis() as u64,
        })));
    }

    fn compacted(&self, c: &Compaction) {
        self.send(SessionUpdate::Other(json!({
            "sessionUpdate": "context_compacted",
            "trigger": c.trigger,
            "tokensBefore": c.tokens_before,
            "tokensAfter": c.tokens_after,
            "prunedResults": c.pruned_results,
            "summarisedMessages": c.summarised_messages,
            "firstKeptCall": c.first_kept_call,
            "summary": c.summary,
            "contextWindow": self.context_window,
        })));
    }
}

/// Retries after a first failed attempt, at most: one per wait in the loop's
/// backoff.
const MAX_RETRIES: usize = crate::agent::BACKOFF.len();

/// How a call ended, as the node reads it.
fn result_update(id: &str, output: &Output) -> ToolCallUpdate {
    ToolCallUpdate {
        tool_call_id: id.into(),
        status: Some(
            if output.is_error {
                "failed"
            } else {
                "completed"
            }
            .into(),
        ),
        raw_output: Some(json!({"output": output.content, "is_error": output.is_error})),
        content: vec![json!({
            "type": "content",
            "content": {"type": "text", "text": output.content},
        })],
        title: None,
        kind: None,
    }
}

/// A tool call as the node sees it. `kind` follows ACP's tool kinds; the node
/// derives what policy reads from the name and input, not from the title.
fn describe(id: &str, name: &str, input: &Value, status: &str) -> ToolCall {
    let path = input["path"].as_str();
    let (kind, title) = match name {
        "read" => ("read", format!("read {}", path.unwrap_or("?"))),
        "edit" | "write" => ("edit", format!("{name} {}", path.unwrap_or("?"))),
        "glob" | "grep" => (
            "search",
            format!("{name} {}", input["pattern"].as_str().unwrap_or("?")),
        ),
        "shell" => (
            "execute",
            input["command"].as_str().unwrap_or("shell").to_string(),
        ),
        subagent::SPAWN => (
            "think",
            format!(
                "subagent: {}",
                subagent::label(input["task"].as_str().unwrap_or_default())
            ),
        ),
        _ => ("other", name.to_string()),
    };
    ToolCall {
        tool_call_id: id.into(),
        title,
        kind: Some(kind.into()),
        status: Some(status.into()),
        raw_input: Some(input.clone()),
        content: Vec::new(),
        locations: path.map(|p| vec![json!({"path": p})]).unwrap_or_default(),
        parent_tool_call_id: None,
    }
}

/// A subagent's call as the node sees it: under the spawning call, and
/// titled with the subagent's task, so its permission card says who asks.
fn child_call(parent: &str, label: &str, id: &str, name: &str, input: &Value) -> ToolCall {
    let mut call = describe(id, name, input, "pending");
    call.title = format!("[{label}] {}", call.title);
    call.parent_tool_call_id = Some(parent.to_string());
    call
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_usage_counts_fresh_input_as_the_gateway_does() {
        let before = Usage {
            input: 100,
            output: 10,
            cache_read: 50,
        };
        let after = Usage {
            input: 1100,
            output: 60,
            cache_read: 850,
        };
        let u = turn_usage(&before, &after);
        assert_eq!(u.input_tokens, 200);
        assert_eq!(u.cached_read_tokens, 800);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.charged(), 250);
    }

    #[test]
    fn instruction_files_follow_the_node_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Use tabs.\n").unwrap();
        let cx = Context::new(dir.path()).unwrap();
        let (system, read) = system_prompt(&cx, Some("Operator: be brief."));
        assert_eq!(read, ["AGENTS.md"]);
        let node = system.find("Operator: be brief.").unwrap();
        let repo = system.find("Use tabs.").unwrap();
        assert!(system.starts_with(ORIENTATION) && node < repo);
    }
}
