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
use crate::runner::{Runner, RunnerCommand};

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
        RunnerCommand {
            argv: vec!["omp".into(), "acp".into()],
            name: name.into(),
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
        let child = runner.spawn(Self::acp_cmd(&spec.container_name)).await?;
        let mut session = OmpSession::start_in(child, &spec.cwd_in_runner).await?;

        // Enforce the pin a second time from the initialize handshake.
        if let Some(v) = &session.agent_version {
            if v != &self.pinned {
                return Err(AdapterError::VersionMismatch {
                    found: v.clone(),
                    pinned: self.pinned.clone(),
                });
            }
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
    session_id: String,
    config_options: Vec<types::ConfigOption>,
    agent_version: Option<String>,
}

impl OmpSession {
    async fn start(child: tokio::process::Child) -> Result<Self, AdapterError> {
        Self::start_in(child, ".").await
    }

    async fn start_in(mut child: tokio::process::Child, cwd: &str) -> Result<Self, AdapterError> {
        let stdin = child.stdin.take().ok_or(AdapterError::NoPipe)?;
        let stdout = child.stdout.take().ok_or(AdapterError::NoPipe)?;
        let (peer, incoming, read) = Peer::new(stdin, stdout);
        tokio::spawn(read);
        // Reap the child when it exits so it is not left as a zombie.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });

        let init: types::InitializeResult = peer
            .request(methods::INITIALIZE, &types::InitializeParams::node())
            .await?;
        let agent_version = init.agent_info.map(|a| a.version);

        let new: types::NewSessionResult = peer
            .request(
                methods::SESSION_NEW,
                &types::NewSessionParams {
                    cwd: cwd.to_string(),
                    mcp_servers: vec![],
                },
            )
            .await?;

        Ok(Self {
            peer,
            incoming,
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
        let res: types::SetConfigOptionResult = self
            .peer
            .request(
                methods::SESSION_SET_CONFIG_OPTION,
                &types::SetConfigOptionParams {
                    session_id: self.session_id.clone(),
                    config_id: "model".into(),
                    value: model.to_string(),
                },
            )
            .await?;
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
    /// the peer closes.
    async fn pump(mut self, tx: mpsc::Sender<HarnessEvent>) {
        while let Some(msg) = self.incoming.recv().await {
            match msg {
                Incoming::Notification { method, params } if method == methods::SESSION_UPDATE => {
                    let p: types::SessionUpdateParams = match serde_json::from_value(params) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!(error=%e, "bad session/update");
                            continue;
                        }
                    };
                    if let Some(ev) = update_to_event(p.update) {
                        if tx.send(ev).await.is_err() {
                            break;
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
                            continue;
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
                        break;
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
        let _ = tx.send(HarnessEvent::Exited { code: None }).await;
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
