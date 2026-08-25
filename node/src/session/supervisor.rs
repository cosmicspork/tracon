//! One task per session: owns the harness handle, translates harness events
//! into persisted events and stream frames, routes permission requests to the
//! queue, and enforces the budget.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::json;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::{
    adapter::{HarnessEvent, HarnessHandle, PermissionReply, PermissionRequest},
    runner::Runner,
    session::{
        chunks::ChunkBuffer,
        state::{event_kind as ek, EndReason, SessionState},
    },
    store::{now_ms, NewEvent, PermissionRow, SessionPatch, Store},
    stream::{Frame, Hub},
};

/// Output of a tool call is capped before it reaches the log; a single read can
/// carry an entire file.
const MAX_TOOL_OUTPUT: usize = 64 * 1024;

/// Commands the HTTP layer sends to a running session.
#[derive(Debug)]
pub enum Command {
    Prompt {
        text: String,
        ack: oneshot::Sender<Result<(), String>>,
    },
    Answer {
        permission_id: String,
        option_id: String,
        ack: oneshot::Sender<Result<(), String>>,
    },
    Kill,
    /// Sent by a turn task when the harness finishes a turn. The supervisor
    /// records it rather than the task, so buffered text is flushed first and
    /// `turn_end` lands after the message it concludes.
    TurnDone {
        kind: &'static str,
        payload: serde_json::Value,
        tokens: i64,
    },
}

pub struct Supervisor {
    pub session_id: String,
    store: Arc<Store>,
    hub: Hub,
    handle: Arc<dyn HarnessHandle>,
    started: Instant,
    permission_timeout: Duration,
    /// Open permission requests, by id, with the channel back to the harness.
    open: Arc<Mutex<HashMap<String, oneshot::Sender<PermissionReply>>>>,
    chunks: ChunkBuffer,
    /// A handle back into this supervisor's own command channel, for turn tasks.
    self_tx: mpsc::Sender<Command>,
    /// Used to force-remove the harness container on teardown. Closing the ACP
    /// session does not necessarily end the harness process, and a container
    /// left running holds the worktree and the credential mounts open.
    runner: Arc<dyn Runner>,
    container: String,
    policy: Arc<crate::policy::Policy>,
    channel: String,
}

impl Supervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: String,
        store: Arc<Store>,
        hub: Hub,
        handle: Arc<dyn HarnessHandle>,
        started: Instant,
        permission_timeout: Duration,
        self_tx: mpsc::Sender<Command>,
        runner: Arc<dyn Runner>,
        container: String,
        policy: Arc<crate::policy::Policy>,
        channel: String,
    ) -> Self {
        Self {
            policy,
            channel,
            self_tx,
            runner,
            container,
            session_id,
            store,
            hub,
            handle,
            started,
            permission_timeout,
            open: Arc::new(Mutex::new(HashMap::new())),
            chunks: ChunkBuffer::default(),
        }
    }

    fn mono_ms(&self) -> i64 {
        self.started.elapsed().as_millis() as i64
    }

    /// Persist an event and publish it on the stream. The stored `seq` is the
    /// SSE id, so a reconnecting client can replay from where it left off.
    fn record(&self, kind: &str, ref_id: Option<String>, payload: serde_json::Value) {
        let e = NewEvent {
            session_id: self.session_id.clone(),
            work_item_id: None,
            kind: kind.to_string(),
            ref_id,
            payload,
            at_ms: now_ms(),
            mono_ms: self.mono_ms(),
        };
        match self.store.append_event(&e) {
            Ok(seq) => self.hub.publish(Frame::Event {
                seq,
                session_id: e.session_id,
                kind: e.kind,
                ref_id: e.ref_id,
                payload: e.payload,
                at_ms: e.at_ms,
            }),
            Err(err) => tracing::error!(error = %err, "failed to persist event"),
        }
    }

    fn set_state(&self, state: SessionState, end_reason: Option<EndReason>) {
        let patch = SessionPatch {
            state: Some(state.as_str().to_string()),
            end_reason: end_reason.map(|r| r.as_str().to_string()),
            ended_mono_ms: state.is_terminal().then(|| self.mono_ms()),
            turn_active: state.is_terminal().then_some(false),
            ..Default::default()
        };
        if let Err(e) = self.store.update_session(&self.session_id, patch) {
            tracing::error!(error = %e, "failed to update session state");
        }
        self.record(
            ek::STATE,
            None,
            json!({ "state": state.as_str(), "end_reason": end_reason.map(|r| r.as_str()) }),
        );
        self.publish_session();
    }

    fn publish_session(&self) {
        if let Ok(Some(row)) = self.store.get_session(&self.session_id) {
            self.hub.publish(Frame::Session(Box::new(row)));
        }
        self.publish_queue();
    }

    fn publish_queue(&self) {
        if let Ok(open) = self.store.open_permissions() {
            self.hub.publish(Frame::Queue { waiting: open });
        }
    }

    /// Drive the session until the harness exits or it is killed.
    pub async fn run(
        mut self,
        mut events: mpsc::Receiver<HarnessEvent>,
        mut commands: mpsc::Receiver<Command>,
    ) {
        self.set_state(SessionState::Running, None);
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut killed_by_us = false;

        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Some(ev) => {
                        if self.on_harness_event(ev).await {
                            break;
                        }
                    }
                    None => break,
                },
                cmd = commands.recv() => match cmd {
                    Some(Command::Prompt { text, ack }) => {
                        let _ = ack.send(self.on_prompt(text).await);
                    }
                    Some(Command::Answer { permission_id, option_id, ack }) => {
                        let _ = ack.send(self.on_answer(&permission_id, &option_id).await);
                    }
                    Some(Command::TurnDone { kind, payload, tokens }) => {
                        self.on_turn_done(kind, payload, tokens).await;
                        if self.check_budget().await {
                            break;
                        }
                    }
                    Some(Command::Kill) => {
                        killed_by_us = true;
                        self.shutdown(EndReason::KilledUser).await;
                        break;
                    }
                    None => break,
                },
                _ = ticker.tick() => self.expire_permissions().await,
            }
        }

        if !killed_by_us {
            self.finish_unexpected().await;
        }
        self.flush_chunks();
        self.remove_container().await;
    }

    async fn on_harness_event(&mut self, ev: HarnessEvent) -> bool {
        match ev {
            HarnessEvent::MessageChunk { message_id, text } => {
                self.hub.publish(Frame::Chunk {
                    session_id: self.session_id.clone(),
                    message_id: message_id.clone(),
                    kind: ek::MESSAGE,
                    text: text.clone(),
                });
                if let Some((kind, id, whole)) = self.chunks.push(ek::MESSAGE, message_id, &text) {
                    self.record(kind, id, json!({ "text": whole }));
                }
            }
            HarnessEvent::ThoughtChunk { message_id, text } => {
                self.hub.publish(Frame::Chunk {
                    session_id: self.session_id.clone(),
                    message_id: message_id.clone(),
                    kind: ek::THOUGHT,
                    text: text.clone(),
                });
                if let Some((kind, id, whole)) = self.chunks.push(ek::THOUGHT, message_id, &text) {
                    self.record(kind, id, json!({ "text": whole }));
                }
            }
            HarnessEvent::ToolCall(t) => {
                self.flush_chunks();
                self.record(
                    ek::TOOL_CALL,
                    Some(t.tool_call_id.clone()),
                    json!({
                        "title": t.title, "kind": t.kind, "status": t.status,
                        "raw_input": t.raw_input, "locations": t.locations
                    }),
                );
            }
            HarnessEvent::ToolCallUpdate(t) => {
                // Updates are cumulative; only the terminal one is worth keeping.
                self.hub.publish(Frame::ToolUpdate {
                    session_id: self.session_id.clone(),
                    tool_call_id: t.tool_call_id.clone(),
                    status: t.status.clone(),
                });
                if t.is_terminal() {
                    let (output, truncated) = truncate(t.raw_output.as_ref());
                    self.record(
                        ek::TOOL_RESULT,
                        Some(t.tool_call_id.clone()),
                        json!({ "status": t.status, "output": output, "truncated": truncated }),
                    );
                }
            }
            HarnessEvent::Plan(p) => {
                self.flush_chunks();
                self.record(ek::PLAN, None, p);
            }
            HarnessEvent::Usage {
                size,
                used,
                cost_usd,
            } => {
                let _ = self.store.update_session(
                    &self.session_id,
                    SessionPatch {
                        cost_usd,
                        context_used: used.map(|u| u as i64),
                        context_size: size.map(|s| s as i64),
                        ..Default::default()
                    },
                );
                self.record(
                    ek::USAGE,
                    None,
                    json!({ "size": size, "used": used, "cost_usd": cost_usd }),
                );
                self.publish_session();
            }
            HarnessEvent::Permission { request, reply } => {
                self.flush_chunks();
                self.on_permission(request, reply).await;
            }
            HarnessEvent::Models(_) | HarnessEvent::Other(_) => {}
            HarnessEvent::Exited { code } => {
                self.record(ek::ERROR, None, json!({ "harness_exit_code": code }));
                return true;
            }
        }
        false
    }

    fn flush_chunks(&mut self) {
        let mut pending = Vec::new();
        self.chunks
            .flush_all(|kind, id, text| pending.push((kind, id, text)));
        for (kind, id, text) in pending {
            self.record(kind, id, json!({ "text": text }));
        }
    }

    async fn on_permission(
        &mut self,
        request: PermissionRequest,
        reply: oneshot::Sender<PermissionReply>,
    ) {
        // Policy first. An auto-answered request never reaches the queue, so
        // the operator is interrupted only by what actually needs them.
        let command = request
            .raw_input
            .as_ref()
            .and_then(|v| v.get("command"))
            .and_then(|c| c.as_str())
            .map(str::to_string);
        let decision = self.policy.decide(&crate::policy::Request {
            channel: &self.channel,
            kind: request.kind.as_deref(),
            title: &request.title,
            command: command.as_deref(),
        });
        match decision.verdict {
            crate::policy::Verdict::Allow | crate::policy::Verdict::Deny => {
                let allow = decision.verdict == crate::policy::Verdict::Allow;
                let option = if allow {
                    crate::acp::types::OPTION_ALLOW_ONCE
                } else {
                    crate::acp::types::OPTION_REJECT_ONCE
                };
                let _ = reply.send(PermissionReply::Selected(option.into()));
                self.record(
                    if allow {
                        ek::POLICY_ALLOWED
                    } else {
                        ek::POLICY_DENIED
                    },
                    None,
                    json!({
                        "title": request.title,
                        "kind": request.kind,
                        "rule": decision.rule_id,
                        "reason": decision.reason,
                    }),
                );
                return;
            }
            crate::policy::Verdict::Ask => {}
        }

        let id = uuid::Uuid::now_v7().to_string();
        let row = PermissionRow {
            id: id.clone(),
            session_id: self.session_id.clone(),
            node_id: String::new(),
            rpc_id: 0,
            tool_call_id: request.tool_call_id.clone(),
            title: request.title.clone(),
            kind: request.kind.clone(),
            raw_input: request
                .raw_input
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default()),
            options: serde_json::to_string(&request.options).unwrap_or_else(|_| "[]".into()),
            state: "new".into(),
            answer_option_id: None,
            created_ms: now_ms(),
            created_mono_ms: self.mono_ms(),
            resolved_mono_ms: None,
            expires_ms: now_ms() + self.permission_timeout.as_millis() as i64,
        };
        if let Err(e) = self.store.insert_permission(&row) {
            tracing::error!(error = %e, "failed to record permission request");
            let _ = reply.send(PermissionReply::Selected(
                crate::acp::types::OPTION_REJECT_ONCE.into(),
            ));
            return;
        }
        self.open.lock().await.insert(id.clone(), reply);
        self.record(
            ek::PERMISSION_REQUEST,
            Some(id.clone()),
            json!({
                "permission_id": id, "title": request.title, "kind": request.kind,
                "raw_input": request.raw_input, "options": request.options,
                "expires_ms": row.expires_ms
            }),
        );
        self.set_state(SessionState::WaitingOnYou, None);
    }

    async fn on_answer(&mut self, permission_id: &str, option_id: &str) -> Result<(), String> {
        let sender = self.open.lock().await.remove(permission_id);
        let Some(sender) = sender else {
            return Err("no open permission request with that id".into());
        };
        if !self
            .store
            .resolve_permission(permission_id, "answered", Some(option_id), self.mono_ms())
            .map_err(|e| e.to_string())?
        {
            return Err("permission request is no longer open".into());
        }
        let _ = sender.send(PermissionReply::Selected(option_id.to_string()));
        self.record(
            ek::PERMISSION_ANSWER,
            Some(permission_id.to_string()),
            json!({ "permission_id": permission_id, "option_id": option_id }),
        );
        self.back_to_running().await;
        Ok(())
    }

    /// Deny-by-default: an unanswered request is rejected once, and the log
    /// says so at the point the harness asked.
    async fn expire_permissions(&mut self) {
        let now = now_ms();
        let due: Vec<String> = match self.store.open_permissions() {
            Ok(rows) => rows
                .into_iter()
                .filter(|r| r.session_id == self.session_id && r.expires_ms <= now)
                .map(|r| r.id)
                .collect(),
            Err(_) => return,
        };
        if due.is_empty() {
            // Nothing expired this tick; do not republish the queue on every
            // idle heartbeat.
            return;
        }
        for id in due {
            if let Some(sender) = self.open.lock().await.remove(&id) {
                let _ = sender.send(PermissionReply::Selected(
                    crate::acp::types::OPTION_REJECT_ONCE.into(),
                ));
            }
            let _ = self
                .store
                .resolve_permission(&id, "expired", None, self.mono_ms());
            self.record(
                ek::PERMISSION_EXPIRED,
                Some(id.clone()),
                json!({ "permission_id": id, "reason": "denied: unanswered" }),
            );
        }
        self.back_to_running().await;
    }

    async fn back_to_running(&mut self) {
        let still_open = self
            .store
            .open_permissions()
            .map(|r| r.iter().any(|p| p.session_id == self.session_id))
            .unwrap_or(false);
        if !still_open {
            if let Ok(Some(s)) = self.store.get_session(&self.session_id) {
                if s.state == SessionState::WaitingOnYou.as_str() {
                    self.set_state(SessionState::Running, None);
                    return;
                }
            }
        }
        self.publish_queue();
    }

    async fn on_prompt(&mut self, text: String) -> Result<(), String> {
        let s = self
            .store
            .get_session(&self.session_id)
            .map_err(|e| e.to_string())?
            .ok_or("session is gone")?;
        if s.turn_active != 0 {
            return Err("a turn is already running".into());
        }
        if s.tokens_used >= s.budget_tokens {
            return Err("session is over budget".into());
        }
        if s.state != SessionState::Running.as_str() {
            return Err(format!("session is {}", s.state));
        }
        let _ = self.store.update_session(
            &self.session_id,
            SessionPatch {
                turn_active: Some(true),
                ..Default::default()
            },
        );
        let _ = self.store.set_draft(&self.session_id, None);
        self.record(ek::USER_PROMPT, None, json!({ "text": text }));
        self.publish_session();

        let handle = self.handle.clone();
        let done = self.self_tx.clone();
        // The turn resolves when the harness stops; it outlives this command.
        tokio::spawn(async move {
            let (kind, payload, tokens) = match handle.prompt(text).await {
                Ok(turn) => (
                    ek::TURN_END,
                    json!({
                        "stop_reason": turn.stop_reason,
                        "usage": {
                            "input_tokens": turn.usage.input_tokens,
                            "output_tokens": turn.usage.output_tokens,
                            "total_tokens": turn.usage.total_tokens,
                            "cached_read_tokens": turn.usage.cached_read_tokens,
                        }
                    }),
                    turn.usage.charged() as i64,
                ),
                Err(e) => (ek::ERROR, json!({ "error": e.to_string() }), 0),
            };
            let _ = done
                .send(Command::TurnDone {
                    kind,
                    payload,
                    tokens,
                })
                .await;
        });
        Ok(())
    }

    /// Record the end of a turn: flush whatever text was still streaming, log
    /// the outcome, and add the turn's tokens to the session's total.
    async fn on_turn_done(&mut self, kind: &'static str, payload: serde_json::Value, tokens: i64) {
        self.flush_chunks();
        let previous = self
            .store
            .get_session(&self.session_id)
            .ok()
            .flatten()
            .map(|s| s.tokens_used)
            .unwrap_or(0);
        let _ = self.store.update_session(
            &self.session_id,
            SessionPatch {
                turn_active: Some(false),
                tokens_used: Some(previous + tokens),
                ..Default::default()
            },
        );
        self.record(kind, None, payload);
        self.publish_session();
    }

    /// Budget is checked at turn end: the harness reports usage per turn, so a
    /// single long turn can overshoot. Enforced by ending the session.
    async fn check_budget(&mut self) -> bool {
        let Ok(Some(s)) = self.store.get_session(&self.session_id) else {
            return false;
        };
        if s.tokens_used >= s.budget_tokens && !SessionState::from_stored(&s.state).is_terminal() {
            self.shutdown(EndReason::Budget).await;
            return true;
        }
        false
    }

    /// Teardown always removes the container. The worktree is left alone: it
    /// holds the session's work.
    async fn remove_container(&self) {
        if let Err(e) = self.runner.kill(&self.container).await {
            tracing::warn!(container = %self.container, error = %e, "could not remove harness container");
        }
    }

    async fn shutdown(&mut self, reason: EndReason) {
        let _ = self.handle.cancel().await;
        let _ = tokio::time::timeout(Duration::from_secs(5), self.handle.close()).await;
        self.remove_container().await;
        self.reject_open_permissions().await;
        let state = match reason {
            EndReason::Budget => SessionState::KilledBudget,
            EndReason::Error => SessionState::Failed,
            _ => SessionState::Closed,
        };
        self.set_state(state, Some(reason));
    }

    async fn finish_unexpected(&mut self) {
        let Ok(Some(s)) = self.store.get_session(&self.session_id) else {
            return;
        };
        if SessionState::from_stored(&s.state).is_terminal() {
            return;
        }
        self.reject_open_permissions().await;
        if s.tokens_used >= s.budget_tokens {
            self.set_state(SessionState::KilledBudget, Some(EndReason::Budget));
        } else {
            self.set_state(SessionState::Closed, Some(EndReason::HarnessExit));
        }
    }

    async fn reject_open_permissions(&mut self) {
        let ids: Vec<String> = self.open.lock().await.keys().cloned().collect();
        for id in ids {
            if let Some(sender) = self.open.lock().await.remove(&id) {
                let _ = sender.send(PermissionReply::Cancelled);
            }
            let _ = self
                .store
                .resolve_permission(&id, "expired", None, self.mono_ms());
            self.record(
                ek::PERMISSION_EXPIRED,
                Some(id.clone()),
                json!({ "permission_id": id, "reason": "denied: session ended" }),
            );
        }
        self.publish_queue();
    }
}

impl SessionState {
    /// Parse a stored state string. Deliberately infallible: an unrecognised
    /// value means the row predates a rename, and treating it as ended is safer
    /// than refusing to read the session at all.
    pub fn from_stored(s: &str) -> Self {
        match s {
            "starting" => Self::Starting,
            "running" => Self::Running,
            "waiting_on_you" => Self::WaitingOnYou,
            "waiting_on_check" => Self::WaitingOnCheck,
            "killed_budget" => Self::KilledBudget,
            "failed" => Self::Failed,
            _ => Self::Closed,
        }
    }
}

fn truncate(v: Option<&serde_json::Value>) -> (Option<String>, bool) {
    let Some(v) = v else { return (None, false) };
    let s = serde_json::to_string(v).unwrap_or_default();
    if s.len() > MAX_TOOL_OUTPUT {
        // Cut on a char boundary: `serde_json` does not escape non-ASCII, so a
        // multibyte character can straddle the cap. Slicing mid-character would
        // panic and take the supervisor task — and the session's cleanup — with
        // it. `floor_char_boundary` is unstable, so walk down by hand.
        let mut cut = MAX_TOOL_OUTPUT;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        (Some(s[..cut].to_string()), true)
    } else {
        (Some(s), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_is_capped() {
        let big = json!({ "text": "x".repeat(MAX_TOOL_OUTPUT * 2) });
        let (out, truncated) = truncate(Some(&big));
        assert!(truncated);
        assert_eq!(out.unwrap().len(), MAX_TOOL_OUTPUT);
        let (small, truncated) = truncate(Some(&json!({ "text": "ok" })));
        assert!(!truncated);
        assert!(small.unwrap().contains("ok"));
    }

    #[test]
    fn a_multibyte_character_on_the_cap_does_not_panic() {
        // `serde_json` emits non-ASCII raw, so a multibyte char can straddle the
        // cap. This must truncate on a boundary, not panic. `é` is two bytes;
        // padding so the cap lands inside one exercises the boundary walk.
        let pad = MAX_TOOL_OUTPUT - "{\"text\":\"".len();
        let text = format!("{}{}", "a".repeat(pad), "é".repeat(64));
        let (out, truncated) = truncate(Some(&json!({ "text": text })));
        assert!(truncated);
        let out = out.unwrap();
        assert!(out.len() <= MAX_TOOL_OUTPUT);
        // It is still valid UTF-8 (the assertion is that we got here at all).
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn unknown_state_strings_read_as_closed() {
        assert_eq!(SessionState::from_stored("running"), SessionState::Running);
        assert_eq!(SessionState::from_stored("nonsense"), SessionState::Closed);
    }
}
