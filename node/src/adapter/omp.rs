//! The omp ACP adapter. Drives `omp acp` over the runner's stdio.

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::{
    AdapterError, HarnessAdapter, HarnessEvent, HarnessHandle, HarnessVersion, LaunchSpec,
    ModelOption, PermissionReply, PermissionRequest, TurnResult,
};
use crate::acp::{
    rpc::{Incoming, Peer},
    types::{self, methods},
};
use crate::runner::{Runner, RunnerCommand, Spawned};

pub struct OmpAdapter {
    pinned: String,
}

impl OmpAdapter {
    pub fn new(pinned: impl Into<String>) -> Self {
        Self {
            pinned: pinned.into(),
        }
    }

    fn acp_cmd(name: &str) -> RunnerCommand {
        Self::acp_cmd_with(name, &[])
    }

    fn acp_cmd_with(name: &str, tools: &[String]) -> RunnerCommand {
        let mut argv = vec!["omp".to_string()];
        if !tools.is_empty() {
            argv.push(format!("--tools={}", tools.join(",")));
        }
        argv.push("acp".into());
        RunnerCommand {
            argv,
            name: name.into(),
            // Explicit even though the image sets it: the mount target and the
            // harness's idea of its state directory must agree.
            env: vec![(
                "OMP_STATE_DIR".into(),
                crate::session::materialize::HARNESS_STATE_TARGET.into(),
            )],
            ..Default::default()
        }
    }
}

fn parse_version(s: &str) -> String {
    // `omp/18.0.4` -> `18.0.4`
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

#[async_trait]
impl HarnessAdapter for OmpAdapter {
    fn id(&self) -> &'static str {
        "omp"
    }

    fn pinned_version(&self) -> &str {
        &self.pinned
    }

    async fn version(&self, runner: &dyn Runner) -> Result<HarnessVersion, AdapterError> {
        let out = runner
            .run_capture(RunnerCommand {
                argv: vec!["omp".into(), "--version".into()],
                name: "omp-version".into(),
                ..Default::default()
            })
            .await?;
        let found = parse_version(&String::from_utf8_lossy(&out.stdout));
        Ok(HarnessVersion {
            found,
            pinned: self.pinned.clone(),
        })
    }

    async fn probe_models(&self, runner: &dyn Runner) -> Result<Vec<ModelOption>, AdapterError> {
        let child = runner.spawn(Self::acp_cmd("omp-probe")).await?;
        let mut session = OmpSession::start(child).await?;
        let models = session.model_options();
        session.close().await.ok();
        Ok(models)
    }

    async fn launch(
        &self,
        runner: &dyn Runner,
        spec: LaunchSpec,
    ) -> Result<(Box<dyn HarnessHandle>, mpsc::Receiver<HarnessEvent>), AdapterError> {
        let child = runner
            .spawn(Self::acp_cmd_with(&spec.container_name, &spec.tools))
            .await?;
        let mut session =
            OmpSession::start_in(child, &spec.cwd_in_runner, spec.mcp_servers.clone()).await?;

        // Enforce the pin a second time from the initialize handshake. A missing
        // version is a mismatch, not a pass: this layer is the most likely to
        // break silently, so it fails closed when it cannot verify.
        let found = session.agent_version.as_deref().unwrap_or("unknown");
        if found != self.pinned {
            return Err(AdapterError::VersionMismatch {
                found: found.to_string(),
                pinned: self.pinned.clone(),
            });
        }
        session.set_model(&spec.model).await?;

        let (event_tx, event_rx) = mpsc::channel(256);
        // Emit the model list first so the node can record it.
        let _ = event_tx
            .send(HarnessEvent::Models(session.model_options()))
            .await;

        let sid = session.session_id.clone();
        let peer = session.peer.clone();
        let handle = OmpHandle {
            peer,
            session_id: sid,
        };
        tokio::spawn(session.pump(event_tx));
        Ok((Box::new(handle), event_rx))
    }
}

/// Owns the peer and the inbound stream between launch and pump.
struct OmpSession {
    peer: Peer,
    incoming: mpsc::Receiver<Incoming>,
    /// Inbound notifications and requests that arrived during the handshake,
    /// before the pump existed. Replayed in order when it starts.
    buffered: Vec<Incoming>,
    session_id: String,
    config_options: Vec<types::ConfigOption>,
    agent_version: Option<String>,
}

/// Await a handshake request while draining inbound traffic into `buffered`.
///
/// The read loop delivers responses and inbound notifications from one place, so
/// a startup that emits more than the inbound channel holds (session/update,
/// available_commands_update, …) before answering would block the read loop on a
/// full channel and the response would never arrive — a deadlock. Draining
/// concurrently keeps the channel moving; the buffered items are replayed to the
/// pump in order, and normal backpressure resumes once the pump owns the stream.
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
                // The peer's stream closed; the request will resolve (with an
                // error) on its own.
                None => return fut.await,
            },
        }
    }
}

impl OmpSession {
    async fn start(child: Spawned) -> Result<Self, AdapterError> {
        // ACP requires an absolute cwd; the image's workdir is the safe choice
        // for a probe that mounts no worktree.
        Self::start_in(child, "/work", Vec::new()).await
    }

    async fn start_in(
        child: Spawned,
        cwd: &str,
        mcp_servers: Vec<Value>,
    ) -> Result<Self, AdapterError> {
        let (peer, mut incoming, read) = Peer::new(child.stdin, child.stdout);
        tokio::spawn(read);
        // Await the exit so a child process is reaped and not left as a zombie.
        tokio::spawn(child.done);

        // Drain inbound traffic while the handshake requests are in flight, so a
        // chatty startup cannot deadlock the read loop (see `drain_until`).
        let mut buffered = Vec::new();
        let init: types::InitializeResult = drain_until(
            &mut incoming,
            &mut buffered,
            peer.request(methods::INITIALIZE, &types::InitializeParams::node()),
        )
        .await?;
        let agent_version = init.agent_info.map(|a| a.version);

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
        .await?;

        Ok(Self {
            peer,
            incoming,
            buffered,
            session_id: new.session_id,
            config_options: new.config_options,
            agent_version,
        })
    }

    fn model_options(&self) -> Vec<ModelOption> {
        types::model_choices(&self.config_options)
            .map(|c| {
                c.options
                    .iter()
                    .map(|o| ModelOption {
                        value: o.value.clone(),
                        name: if o.name.is_empty() {
                            o.value.clone()
                        } else {
                            o.name.clone()
                        },
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn set_model(&mut self, model: &str) -> Result<(), AdapterError> {
        let known = types::model_choices(&self.config_options)
            .map(|c| c.options.iter().any(|o| o.value == model))
            .unwrap_or(false);
        if !known {
            return Err(AdapterError::UnknownModel(model.to_string()));
        }
        let params = types::SetConfigOptionParams {
            session_id: self.session_id.clone(),
            config_id: "model".into(),
            value: model.to_string(),
        };
        let req = self
            .peer
            .request(methods::SESSION_SET_CONFIG_OPTION, &params);
        let res: types::SetConfigOptionResult =
            drain_until(&mut self.incoming, &mut self.buffered, req).await?;
        if !res.config_options.is_empty() {
            self.config_options = res.config_options;
        }
        Ok(())
    }

    async fn close(&mut self) -> Result<(), AdapterError> {
        let _: Value = self
            .peer
            .request(
                methods::SESSION_CLOSE,
                &types::SessionIdParams {
                    session_id: self.session_id.clone(),
                },
            )
            .await?;
        Ok(())
    }

    /// Translate inbound notifications and requests into `HarnessEvent`s until
    /// the peer closes. Handshake traffic buffered before the pump existed is
    /// replayed first, in order.
    async fn pump(mut self, tx: mpsc::Sender<HarnessEvent>) {
        for msg in std::mem::take(&mut self.buffered) {
            if !Self::process(msg, &tx).await {
                return;
            }
        }
        while let Some(msg) = self.incoming.recv().await {
            if !Self::process(msg, &tx).await {
                return;
            }
        }
        let _ = tx.send(HarnessEvent::Exited { code: None }).await;
    }

    /// Handle one inbound message. Returns false when the pump should stop (the
    /// downstream event channel closed).
    async fn process(msg: Incoming, tx: &mpsc::Sender<HarnessEvent>) -> bool {
        {
            match msg {
                Incoming::Notification { method, params } if method == methods::SESSION_UPDATE => {
                    let p: types::SessionUpdateParams = match serde_json::from_value(params) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!(error=%e, "bad session/update");
                            return true;
                        }
                    };
                    if let Some(ev) = update_to_event(p.update) {
                        if tx.send(ev).await.is_err() {
                            return false;
                        }
                    }
                }
                Incoming::Notification { .. } => {}
                Incoming::Request {
                    method,
                    params,
                    reply,
                    ..
                } if method == methods::SESSION_REQUEST_PERMISSION => {
                    let rp: types::RequestPermissionParams = match serde_json::from_value(params) {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = reply.send(Err(crate::acp::codec::RpcError::method_not_found(
                                format!("bad permission request: {e}"),
                            )));
                            return true;
                        }
                    };
                    let (answer_tx, answer_rx) = oneshot::channel();
                    let request = PermissionRequest {
                        tool_call_id: Some(rp.tool_call.tool_call_id.clone()),
                        title: rp.tool_call.title.clone(),
                        kind: rp.tool_call.kind.clone(),
                        raw_input: rp.tool_call.raw_input.clone(),
                        options: rp.options,
                    };
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
                    // Answer the harness once the supervisor decides.
                    tokio::spawn(async move {
                        let outcome = match answer_rx.await {
                            Ok(PermissionReply::Selected(opt)) => selected_outcome(&opt),
                            _ => cancelled_outcome(),
                        };
                        let _ = reply.send(Ok(outcome));
                    });
                }
                Incoming::Request { method, reply, .. } => {
                    // fs/*, terminal/*: the node declared these unavailable.
                    let _ = reply.send(Err(crate::acp::codec::RpcError::method_not_found(method)));
                }
            }
        }
        true
    }
}

fn selected_outcome(option_id: &str) -> Value {
    serde_json::json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
}

fn cancelled_outcome() -> Value {
    serde_json::json!({ "outcome": { "outcome": "cancelled" } })
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
        U::ConfigOptionUpdate(c) => HarnessEvent::Models(
            types::model_choices(&c.config_options)
                .map(|opt| {
                    opt.options
                        .iter()
                        .map(|o| ModelOption {
                            value: o.value.clone(),
                            name: if o.name.is_empty() {
                                o.value.clone()
                            } else {
                                o.name.clone()
                            },
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ),
        U::SessionInfoUpdate(_) | U::AvailableCommandsUpdate(_) => return None,
        U::Other(v) => HarnessEvent::Other(v),
    })
}

fn chunk_text(content: types::ContentBlock) -> String {
    match content {
        types::ContentBlock::Text { text } => text,
        types::ContentBlock::Other => String::new(),
    }
}

struct OmpHandle {
    peer: Peer,
    session_id: String,
}

#[async_trait]
impl HarnessHandle for OmpHandle {
    fn harness_session_id(&self) -> &str {
        &self.session_id
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
            .await?;
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
            .await?;
        Ok(())
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
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restricted_tool_list_reaches_the_harness() {
        // A tool that is absent cannot be talked into running, so the surface
        // is reduced before the gate has to decide anything.
        let cmd = OmpAdapter::acp_cmd_with("c", &["read".to_string(), "list".to_string()]);
        assert_eq!(cmd.argv, ["omp", "--tools=read,list", "acp"]);
    }

    #[test]
    fn an_empty_list_leaves_the_harness_default() {
        let cmd = OmpAdapter::acp_cmd_with("c", &[]);
        assert_eq!(cmd.argv, ["omp", "acp"]);
    }
}
