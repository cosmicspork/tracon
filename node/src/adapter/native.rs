//! `tracon-agent`, tracon's own harness, driven over its stdio with the ACP
//! vocabulary in `acp::types`.
//!
//! This is the omp adapter that left at the OpenCode cutover, back for a
//! harness whose protocol is the node's own: no translation, no catalogue to
//! probe, no login. The model, the gateway URL and the key variable reach the
//! harness in a providers file staged per session; the key itself is in the
//! session's secret environment.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use super::{
    AdapterError, HarnessAdapter, HarnessCompat, HarnessEvent, HarnessHandle, HarnessVersion,
    LaunchSpec, Layout, ModelOption, PermissionReply, PermissionRequest, ProtocolSupport, Resume,
    TurnResult,
};
use crate::adapter::types::{self, methods};
use crate::config::SHAPE_OPENAI;
use crate::gateway::model::{key_env_name, Wiring};
use crate::runner::{Runner, RunnerCommand, Spawned};
use acp::codec::RpcError;
use acp::rpc::{Incoming, Peer};

const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub struct NativeAdapter {
    pinned: String,
}

impl NativeAdapter {
    pub const ID: &'static str = "native";

    /// Where the harness image installs it.
    pub const BINARY: &'static str = "/usr/local/bin/tracon-agent";

    /// Built from this workspace on the same release train, so the version
    /// this node drives is its own. The image installs the release asset of
    /// that version, or a binary the operator built from the same tree.
    pub const PINNED_VERSION: &'static str = env!("CARGO_PKG_VERSION");

    pub const PROTOCOL: ProtocolSupport = ProtocolSupport {
        name: "acp",
        min: types::PROTOCOL_MIN,
        max: types::PROTOCOL_MAX,
    };

    /// Relative to the state directory, sealed read-only: what the harness
    /// may reach and how.
    const CONFIG_DIR: &'static str = "config";
    const PROVIDERS_FILE: &'static str = "config/providers.json";
    /// Relative to the state directory, the session's own scratch: the
    /// events file every `session/update` is mirrored to.
    const RUN_DIR: &'static str = "run";

    pub fn new(pinned: impl Into<String>) -> Self {
        Self {
            pinned: pinned.into(),
        }
    }

    /// It reads no variable for its state: every path is on its argv.
    pub const fn layout() -> Layout {
        Layout {
            dir: ".tracon-agent",
            env: "",
        }
    }

    fn cmd(spec: &LaunchSpec) -> RunnerCommand {
        let state = crate::session::materialize::state_target(&spec.harness_home, Self::layout());
        let mut argv = vec![
            Self::BINARY.to_string(),
            "--providers".into(),
            format!("{state}/{}", Self::PROVIDERS_FILE),
            "--model".into(),
            spec.model.clone(),
            "--events".into(),
            format!("{state}/{}/events.jsonl", Self::RUN_DIR),
        ];
        if let Some(file) = &spec.system_prompt_file {
            argv.push("--system-prompt-file".into());
            argv.push(file.clone());
        }
        RunnerCommand {
            argv,
            env: spec.env.clone(),
            secret_env: spec.secret_env.clone(),
            workdir: Some(spec.cwd_in_runner.clone()),
            name: spec.container_name.clone(),
            ..Default::default()
        }
    }
}

/// The providers this harness can speak to, as it reads them: the OpenAI
/// shape only. Anthropic models run on Claude Code, and the Codex backend
/// waits for the Responses encoder.
fn providers_document(wiring: &Wiring) -> Value {
    Value::Array(
        wiring
            .providers
            .iter()
            .filter(|p| p.shape == SHAPE_OPENAI)
            .map(|p| {
                json!({
                    "name": p.name,
                    "shape": p.shape,
                    "base_url": p.base_url,
                    "key_env": key_env_name(&p.name),
                    "models": p.models.iter().map(|m| json!({"id": m.id, "context": m.context})).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

fn parse_version(out: &str) -> String {
    // "tracon-agent 0.34.0"
    out.split_whitespace()
        .last()
        .unwrap_or("unknown")
        .to_string()
}

/// What policy reads, from the tool's own name and input: the same closed
/// mapping the other adapters make, so a rule written for Claude Code's
/// `Read` reads this harness's `read` the same way.
fn permission_target(tool: &str, input: &Value) -> (String, Option<String>, Option<String>) {
    let text = |key: &str| input[key].as_str().map(str::to_string);
    match tool {
        "read" | "edit" | "write" => (tool.to_string(), text("path"), None),
        "glob" | "grep" => (
            tool.to_string(),
            text("path").or_else(|| text("pattern")),
            None,
        ),
        "shell" => ("bash".to_string(), None, text("command")),
        // The node's own tools arrive as `mcp__tracon__<tool>`, as Claude
        // Code spells them; anything else is asked.
        other => (other.to_string(), None, None),
    }
}

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

#[async_trait]
impl HarnessAdapter for NativeAdapter {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn pinned_version(&self) -> &str {
        &self.pinned
    }

    fn protocol(&self) -> ProtocolSupport {
        Self::PROTOCOL
    }

    fn scratch_files(&self, wiring: &Wiring) -> Vec<(String, String)> {
        vec![(
            Self::PROVIDERS_FILE.into(),
            serde_json::to_string_pretty(&providers_document(wiring)).unwrap_or_default(),
        )]
    }

    fn scratch_dirs(&self) -> Vec<String> {
        vec![Self::RUN_DIR.into()]
    }

    fn readonly_dirs(&self) -> Vec<String> {
        vec![Self::CONFIG_DIR.into()]
    }

    /// The agent journals each session, so a new process carries it on with
    /// `session/load`.
    fn restorable(&self) -> bool {
        true
    }

    /// The repository instruction files it reads into its prompt are the
    /// repository's to have; nothing in a workspace configures this harness.
    fn refused_workspace_entries(&self) -> &'static [&'static str] {
        &[]
    }

    async fn version(&self, runner: &dyn Runner) -> Result<HarnessVersion, AdapterError> {
        let out = runner
            .run_probe(RunnerCommand {
                argv: vec![Self::BINARY.into(), "--version".into()],
                name: "native-version".into(),
                ..Default::default()
            })
            .await?;
        Ok(HarnessVersion {
            found: parse_version(&String::from_utf8_lossy(&out.stdout)),
            pinned: self.pinned.clone(),
        })
    }

    /// Declared, never probed: the models this node configures under the
    /// providers it can speak to, spelled `provider/model`.
    async fn probe_models(
        &self,
        _runner: &dyn Runner,
        wiring: &Wiring,
    ) -> Result<Vec<ModelOption>, AdapterError> {
        Ok(wiring
            .providers
            .iter()
            .filter(|p| p.shape == SHAPE_OPENAI)
            .flat_map(|p| {
                p.models.iter().map(|m| ModelOption {
                    value: format!("{}/{}", p.name, m.id),
                    name: m.label().to_string(),
                    harnesses: Vec::new(),
                })
            })
            .collect())
    }

    async fn launch(
        &self,
        runner: &dyn Runner,
        spec: LaunchSpec,
    ) -> Result<(Box<dyn HarnessHandle>, mpsc::Receiver<HarnessEvent>), AdapterError> {
        let child = runner.spawn(Self::cmd(&spec)).await?;
        let session = match tokio::time::timeout(
            START_TIMEOUT,
            Session::start(
                child,
                &spec.cwd_in_runner,
                spec.mcp_servers.clone(),
                spec.resume.as_ref(),
            ),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                let _ =
                    tokio::time::timeout(CLEANUP_TIMEOUT, runner.kill(&spec.container_name)).await;
                return Err(AdapterError::Protocol(
                    "tracon-agent startup timed out".into(),
                ));
            }
        };
        // The pin again, from the handshake. A missing version is a mismatch:
        // this layer fails closed when it cannot verify.
        let found = session.agent_version.as_deref().unwrap_or("unknown");
        if found != self.pinned {
            return Err(AdapterError::VersionMismatch {
                found: found.to_string(),
                pinned: self.pinned.clone(),
            });
        }
        let (event_tx, event_rx) = mpsc::channel(256);
        let (barrier_tx, barrier_rx) = mpsc::channel(8);
        let handle = NativeHandle {
            peer: session.peer.clone(),
            session_id: session.session_id.clone(),
            barrier_tx,
            compat: session.compat(&self.pinned),
            last_seq: session.last_seq.clone(),
        };
        tokio::spawn(session.pump(event_tx, barrier_rx));
        Ok((Box::new(handle), event_rx))
    }
}

/// Owns the peer and the inbound stream between launch and pump.
struct Session {
    peer: Peer,
    incoming: mpsc::Receiver<Incoming>,
    /// Inbound traffic that arrived during the handshake, replayed in order
    /// when the pump starts.
    buffered: Vec<Incoming>,
    session_id: String,
    agent_version: Option<String>,
    agent_name: Option<String>,
    protocol_version: u32,
    /// The highest update sequence taken: an update at or below it was taken
    /// already, from this process or the one it replaced.
    last_seq: Arc<AtomicU64>,
}

/// Await a handshake request while draining inbound traffic into `buffered`,
/// so a startup that says more than the inbound channel holds cannot block the
/// read loop that would deliver the response.
async fn drain_until<R>(
    incoming: &mut mpsc::Receiver<Incoming>,
    buffered: &mut Vec<Incoming>,
    fut: impl std::future::Future<Output = R>,
) -> R {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            r = &mut fut => return r,
            item = incoming.recv() => match item {
                Some(item) => buffered.push(item),
                None => return fut.await,
            },
        }
    }
}

/// The reader and the reaper start before it is known whether the harness
/// will answer. Dropped without `keep`, they are aborted, so a startup that
/// failed leaves nothing still answering for it.
struct StartupTasks {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl StartupTasks {
    fn keep(&mut self) {
        self.tasks.clear();
    }
}

impl Drop for StartupTasks {
    fn drop(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

impl Session {
    async fn start(
        child: Spawned,
        cwd: &str,
        mcp_servers: Vec<Value>,
        resume: Option<&Resume>,
    ) -> Result<Self, AdapterError> {
        let (peer, mut incoming, read) = Peer::new(child.stdin, child.stdout);
        let mut startup = StartupTasks {
            tasks: vec![
                tokio::spawn(read),
                // Await the exit so the child is reaped, not a zombie.
                tokio::spawn(async move {
                    let _ = child.done.await;
                }),
            ],
        };
        let mut buffered = Vec::new();
        let init: types::InitializeResult = drain_until(
            &mut incoming,
            &mut buffered,
            peer.request(methods::INITIALIZE, &types::InitializeParams::node()),
        )
        .await
        .map_err(rpc_error)?;
        let (agent_name, agent_version) = match init.agent_info {
            Some(info) => (Some(info.name), Some(info.version)),
            None => (None, None),
        };
        if !NativeAdapter::PROTOCOL.accepts(init.protocol_version) {
            return Err(AdapterError::IncompatibleProtocol {
                agent: agent_name.unwrap_or_else(|| NativeAdapter::ID.into()),
                protocol: NativeAdapter::PROTOCOL.name,
                found: init.protocol_version.to_string(),
                supported: NativeAdapter::PROTOCOL.versions(),
            });
        }
        let session_id = match resume {
            Some(resume) => {
                if init.extra["agentCapabilities"]["loadSession"] != true {
                    return Err(AdapterError::Protocol(
                        "tracon-agent cannot load a session, so this one cannot be carried on"
                            .into(),
                    ));
                }
                let _: Value = drain_until(
                    &mut incoming,
                    &mut buffered,
                    peer.request(
                        methods::SESSION_LOAD,
                        &types::LoadSessionParams {
                            session_id: resume.harness_session_id.clone(),
                            cwd: cwd.to_string(),
                            mcp_servers,
                        },
                    ),
                )
                .await
                .map_err(rpc_error)?;
                resume.harness_session_id.clone()
            }
            None => {
                let new: types::NewSessionResult = drain_until(
                    &mut incoming,
                    &mut buffered,
                    peer.request(
                        methods::SESSION_NEW,
                        &types::NewSessionParams {
                            cwd: cwd.to_string(),
                            mcp_servers,
                        },
                    ),
                )
                .await
                .map_err(rpc_error)?;
                new.session_id
            }
        };
        startup.keep();
        Ok(Self {
            peer,
            incoming,
            buffered,
            session_id,
            agent_version,
            agent_name,
            protocol_version: init.protocol_version,
            last_seq: Arc::new(AtomicU64::new(resume.map_or(0, |r| r.last_seq))),
        })
    }

    fn compat(&self, pinned: &str) -> HarnessCompat {
        HarnessCompat {
            agent: self
                .agent_name
                .clone()
                .unwrap_or_else(|| NativeAdapter::ID.into()),
            version: self
                .agent_version
                .clone()
                .unwrap_or_else(|| pinned.to_string()),
            protocol: NativeAdapter::PROTOCOL.tag(self.protocol_version),
        }
    }

    /// Translate inbound traffic into `HarnessEvent`s until the peer closes,
    /// handshake traffic first.
    async fn pump(
        mut self,
        tx: mpsc::Sender<HarnessEvent>,
        mut barriers: mpsc::Receiver<oneshot::Sender<()>>,
    ) {
        for msg in std::mem::take(&mut self.buffered) {
            if !process(msg, &tx, &self.last_seq).await {
                return;
            }
        }
        loop {
            tokio::select! {
                msg = self.incoming.recv() => match msg {
                    Some(msg) => {
                        if !process(msg, &tx, &self.last_seq).await {
                            return;
                        }
                    }
                    None => break,
                },
                barrier = barriers.recv() => if let Some(done) = barrier {
                    while let Ok(msg) = self.incoming.try_recv() {
                        if !process(msg, &tx, &self.last_seq).await {
                            return;
                        }
                    }
                    let _ = done.send(());
                },
            }
        }
        let _ = tx.send(HarnessEvent::Exited { code: None }).await;
    }
}

fn rpc_error(e: acp::rpc::RpcClientError) -> AdapterError {
    AdapterError::Protocol(e.to_string())
}

/// Handle one inbound message. False when the event channel closed.
async fn process(msg: Incoming, tx: &mpsc::Sender<HarnessEvent>, last_seq: &AtomicU64) -> bool {
    match msg {
        Incoming::Notification { method, params } if method == methods::SESSION_UPDATE => {
            let p: types::SessionUpdateParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "bad session/update from tracon-agent");
                    return true;
                }
            };
            if let Some(seq) = p.seq {
                if seq <= last_seq.load(Ordering::SeqCst) {
                    return true;
                }
                last_seq.store(seq, Ordering::SeqCst);
            }
            let request = match &p.update {
                types::SessionUpdate::UsageUpdate(u) => u.request.clone(),
                _ => None,
            };
            if let Some(ev) = update_to_event(p.update) {
                if tx.send(ev).await.is_err() {
                    return false;
                }
            }
            match request {
                Some(r) => tx.send(HarnessEvent::RequestUsage(r)).await.is_ok(),
                None => true,
            }
        }
        Incoming::Notification { method, params } => tx
            .send(HarnessEvent::Other(
                json!({ "method": method, "params": params }),
            ))
            .await
            .is_ok(),
        Incoming::Request {
            method,
            params,
            reply,
            ..
        } if method == methods::SESSION_REQUEST_PERMISSION => {
            let rp: types::RequestPermissionParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => {
                    let _ = reply.send(Err(RpcError::method_not_found(format!(
                        "bad permission request: {e}"
                    ))));
                    return true;
                }
            };
            let input = rp.tool_call.raw_input.clone().unwrap_or(Value::Null);
            // A request that names no tool names nothing policy can match.
            let tool = rp.tool.clone().unwrap_or_default();
            let (action, resource, command) = permission_target(&tool, &input);
            let request = PermissionRequest::managed(
                Some(rp.tool_call.tool_call_id.clone()),
                rp.tool_call.title.clone(),
                &action,
                resource,
                command,
                rp.tool_call.raw_input.clone(),
                rp.options,
            );
            let (answer_tx, answer_rx) = oneshot::channel();
            if tx
                .send(HarnessEvent::Permission {
                    request,
                    reply: answer_tx,
                })
                .await
                .is_err()
            {
                let _ = reply.send(Ok(cancelled_outcome()));
                return false;
            }
            tokio::spawn(async move {
                let outcome = match answer_rx.await {
                    Ok(PermissionReply::Selected(option)) => selected_outcome(&option),
                    // Rewritten arguments would have to reach the harness's
                    // own tool call, which ACP cannot carry: refuse rather
                    // than run what the operator did not approve.
                    Ok(PermissionReply::Edited { .. }) => {
                        selected_outcome(types::OPTION_REJECT_ONCE)
                    }
                    // The agent tells its model this, so a policy rule does
                    // not read as the operator's choice.
                    Ok(PermissionReply::Refused { rule, reason }) => {
                        let mut outcome = selected_outcome(types::OPTION_REJECT_ONCE);
                        outcome["message"] =
                            json!(refusal_message(rule.as_deref(), reason.as_deref()));
                        outcome
                    }
                    Ok(PermissionReply::Cancelled) | Err(_) => cancelled_outcome(),
                };
                let _ = reply.send(Ok(outcome));
            });
            true
        }
        Incoming::Request { method, reply, .. } => {
            // fs/*, terminal/*: the node declared these unavailable.
            let _ = reply.send(Err(RpcError::method_not_found(method)));
            true
        }
    }
}

fn selected_outcome(option_id: &str) -> Value {
    json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
}

fn cancelled_outcome() -> Value {
    json!({ "outcome": { "outcome": "cancelled" } })
}

fn update_to_event(u: types::SessionUpdate) -> Option<HarnessEvent> {
    use types::SessionUpdate as U;
    Some(match u {
        U::AgentMessageChunk(c) => HarnessEvent::MessageChunk {
            message_id: c.message_id,
            text: chunk_text(c.content),
        },
        U::AgentThoughtChunk(c) => HarnessEvent::ThoughtChunk {
            message_id: c.message_id,
            text: chunk_text(c.content),
        },
        U::ToolCall(t) => HarnessEvent::ToolCall(t),
        U::ToolCallUpdate(t) => HarnessEvent::ToolCallUpdate(t),
        U::Plan(p) => HarnessEvent::Plan(serde_json::to_value(p).unwrap_or(Value::Null)),
        U::UsageUpdate(u) => HarnessEvent::Usage {
            size: u.size,
            used: u.used,
            cost_usd: u.cost.map(|c| c.amount),
        },
        U::ConfigOptionUpdate(_) | U::SessionInfoUpdate(_) | U::AvailableCommandsUpdate(_) => {
            return None
        }
        U::Other(v) => HarnessEvent::Other(v),
    })
}

fn chunk_text(content: types::ContentBlock) -> String {
    match content {
        types::ContentBlock::Text { text } => text,
        types::ContentBlock::Other => String::new(),
    }
}

struct NativeHandle {
    peer: Peer,
    session_id: String,
    barrier_tx: mpsc::Sender<oneshot::Sender<()>>,
    compat: HarnessCompat,
    last_seq: Arc<AtomicU64>,
}

#[async_trait]
impl HarnessHandle for NativeHandle {
    fn harness_session_id(&self) -> &str {
        &self.session_id
    }

    fn compat(&self) -> HarnessCompat {
        self.compat.clone()
    }

    fn resume_point(&self) -> Option<Resume> {
        Some(Resume {
            harness_session_id: self.session_id.clone(),
            last_seq: self.last_seq.load(Ordering::SeqCst),
        })
    }

    async fn prompt(&self, text: String) -> Result<TurnResult, AdapterError> {
        let res: types::PromptResult = self
            .peer
            .request(
                methods::SESSION_PROMPT,
                &types::PromptParams {
                    session_id: self.session_id.clone(),
                    prompt: vec![types::ContentBlock::Text { text }],
                },
            )
            .await
            .map_err(rpc_error)?;
        Ok(TurnResult {
            stop_reason: res.stop_reason,
            usage: res.usage.unwrap_or_default(),
        })
    }

    async fn cancel(&self) -> Result<(), AdapterError> {
        self.peer
            .notify(
                methods::SESSION_CANCEL,
                &types::SessionIdParams {
                    session_id: self.session_id.clone(),
                },
            )
            .await
            .map_err(rpc_error)
    }

    /// The agent sends every update of a turn before the turn's answer, but
    /// they reach the supervisor through the pump: drain what it holds.
    async fn quiesce_events(&self) -> Result<(), AdapterError> {
        let (done, wait) = oneshot::channel();
        self.barrier_tx
            .send(done)
            .await
            .map_err(|_| AdapterError::Protocol("tracon-agent event pump stopped".into()))?;
        wait.await
            .map_err(|_| AdapterError::Protocol("tracon-agent event barrier stopped".into()))
    }

    async fn close(&self) -> Result<(), AdapterError> {
        let _: Value = self
            .peer
            .request(
                methods::SESSION_CLOSE,
                &types::SessionIdParams {
                    session_id: self.session_id.clone(),
                },
            )
            .await
            .map_err(rpc_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_reads_the_same_targets_as_for_the_other_harnesses() {
        let read = PermissionRequest::managed(
            None,
            "read src/a.rs".into(),
            "read",
            Some("src/a.rs".into()),
            None,
            None,
            Vec::new(),
        );
        let (action, resource, command) = permission_target("read", &json!({"path": "src/a.rs"}));
        assert_eq!(
            (action.as_str(), resource.clone(), command.clone()),
            ("read", read.resource, None)
        );
        let (action, resource, command) =
            permission_target("shell", &json!({"command": "cargo test"}));
        let shell = PermissionRequest::managed(
            None,
            String::new(),
            &action,
            resource,
            command,
            None,
            Vec::new(),
        );
        assert_eq!(shell.action, "bash");
        assert_eq!(shell.kind.as_deref(), Some("execute"));
        assert_eq!(shell.command.as_deref(), Some("cargo test"));
        let (action, ..) = permission_target("edit", &json!({"path": "a"}));
        let edit = PermissionRequest::managed(
            None,
            String::new(),
            &action,
            Some("a".into()),
            None,
            None,
            Vec::new(),
        );
        assert_eq!(edit.kind.as_deref(), Some("write"));
        // A node tool keeps the spelling the supervisor recognises.
        let (action, resource, command) = permission_target("mcp__tracon__doc_read", &json!({}));
        assert_eq!(
            (action.as_str(), resource, command),
            ("mcp__tracon__doc_read", None, None)
        );
        // An unnamed or unknown tool has no kind, so no allow rule matches it.
        let unknown =
            PermissionRequest::managed(None, String::new(), "", None, None, None, Vec::new());
        assert_eq!(unknown.kind, None);
    }

    /// What reaches the agent for a policy refusal: a rejection that says
    /// which rule, so its model does not read it as the operator's choice.
    #[test]
    fn a_refusal_carries_the_rule_and_reason() {
        let mut outcome = selected_outcome(types::OPTION_REJECT_ONCE);
        outcome["message"] = json!(refusal_message(
            Some("review-before-publish"),
            Some("commit, then submit_review")
        ));
        let parsed: types::RequestPermissionResult = serde_json::from_value(outcome).unwrap();
        assert_eq!(
            parsed.message.as_deref(),
            Some("the node's policy refused this (rule `review-before-publish`): commit, then submit_review")
        );
    }

    #[test]
    fn the_providers_file_holds_only_what_it_can_speak() {
        let mut cfg = crate::config::Config::default();
        cfg.providers.insert(
            "local".into(),
            crate::config::Provider {
                credential: "local".into(),
                upstream: "http://127.0.0.1:8080".into(),
                shape: SHAPE_OPENAI.into(),
                models: vec![crate::config::ModelDecl {
                    id: "qwen3-8b".into(),
                    context: 32768,
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        let wiring = crate::gateway::model::harness_wiring(&cfg, "tracon-gw", "tok", |_, _| true);
        let files = NativeAdapter::new("0").scratch_files(&wiring);
        assert_eq!(files[0].0, "config/providers.json");
        let doc: Value = serde_json::from_str(&files[0].1).unwrap();
        let local = doc
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "local")
            .unwrap();
        assert_eq!(
            local["base_url"],
            format!("http://tracon-gw:{}/model/local", cfg.gateway.forward_port)
        );
        assert_eq!(local["key_env"], "TRACON_PROVIDER_KEY_LOCAL");
        assert_eq!(
            local["models"][0],
            json!({"id": "qwen3-8b", "context": 32768})
        );
        assert!(doc
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["shape"] == SHAPE_OPENAI));
        // The token is never in the file: it travels as secret environment.
        assert!(!files[0].1.contains("tok\""));
    }

    #[test]
    fn the_argv_names_every_path_and_no_secret() {
        let spec = LaunchSpec {
            cwd_in_runner: "/work".into(),
            model: "local/qwen3-8b".into(),
            container_name: "c".into(),
            harness_home: "/root".into(),
            mcp_servers: Vec::new(),
            tools: Vec::new(),
            env: Vec::new(),
            secret_env: Default::default(),
            system_prompt_file: Some("/root/.tracon/orientation.md".into()),
            cursor: None,
            resume: None,
        };
        let cmd = NativeAdapter::cmd(&spec);
        assert_eq!(
            cmd.argv,
            [
                "/usr/local/bin/tracon-agent",
                "--providers",
                "/root/.tracon-agent/config/providers.json",
                "--model",
                "local/qwen3-8b",
                "--events",
                "/root/.tracon-agent/run/events.jsonl",
                "--system-prompt-file",
                "/root/.tracon/orientation.md",
            ]
        );
        assert_eq!(cmd.workdir.as_deref(), Some("/work"));
        assert_eq!(parse_version("tracon-agent 0.34.0\n"), "0.34.0");
    }
}
