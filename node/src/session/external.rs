//! A harness the operator runs themselves, attached to a channel.
//!
//! There is no process here to supervise: the harness is a terminal the
//! operator started, outside the boundary, and it reaches the node through the
//! operator door rather than the gateway's forward. What it still needs is a
//! session, because that is where a brokered call the policy does not name
//! becomes a card on the home: the queue is keyed on a session row, the answer
//! comes back through the same command channel, and the log of what was asked
//! for belongs somewhere the operator can read it afterwards.
//!
//! So a channel gets one attached session and a loop that handles only what an
//! attachment can produce: a permission request, its answer, its expiry, and
//! the end. No turns, no budget, no container. A `Supervisor` with a stubbed
//! harness would have to fake all three.

use std::{collections::HashMap, sync::Arc, time::Duration, time::Instant};

use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use crate::{
    adapter::{PermissionReply, PermissionRequest},
    session::{
        state::{event_kind as ek, EndReason, SessionState},
        supervisor::{on_answer_row, permission_row, Command},
    },
    store::{now_ms, NewEvent, SessionPatch, Store},
    stream::{Bus, Frame},
};

/// The `harness_id` an attached session records. Not an adapter: nothing is
/// launched, and `adapter_for` never sees it. The interface keys its wording
/// off this, and `recent_repos` filters on it.
pub const HARNESS_ID: &str = "external";

/// One attachment per channel, while it is live.
pub(super) struct Attachment {
    pub session_id: String,
    /// Bumped by every call the door lets through; the loop reads it on its
    /// tick to decide whether the harness is still there.
    pub last_seen: Arc<std::sync::Mutex<Instant>>,
}

pub(super) struct Loop {
    pub session_id: String,
    pub node_id: String,
    pub store: Arc<Store>,
    pub bus: Bus,
    pub permission_timeout: Duration,
    pub idle_timeout: Duration,
    pub last_seen: Arc<std::sync::Mutex<Instant>>,
}

impl Loop {
    /// Until the attachment is killed, goes quiet, or the node stops.
    pub(super) async fn run(self, mut commands: mpsc::Receiver<Command>) {
        let started = Instant::now();
        let mut open: HashMap<String, oneshot::Sender<PermissionReply>> = HashMap::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    Some(Command::Permission { request, reply }) => {
                        self.on_permission(&mut open, started, request, reply);
                    }
                    Some(Command::Answer { permission_id, option_id, arguments, ack }) => {
                        let done = on_answer_row(
                            &self.store,
                            &mut open,
                            &permission_id,
                            &option_id,
                            arguments.clone(),
                            started.elapsed().as_millis() as i64,
                        );
                        if done.is_ok() {
                            self.record(
                                ek::PERMISSION_ANSWER,
                                Some(permission_id.clone()),
                                json!({ "permission_id": permission_id, "option_id": option_id, "arguments": arguments }),
                                started,
                            );
                            self.back_to_running(&open);
                        }
                        let _ = ack.send(done);
                    }
                    Some(Command::Prompt { ack, .. }) => {
                        // The operator is already talking to this harness; the
                        // node is not in that conversation.
                        let _ = ack.send(Err(
                            "this session is a harness you run yourself; prompt it in its own terminal"
                                .into(),
                        ));
                    }
                    Some(Command::Kill) => {
                        self.close(&mut open, started, EndReason::KilledUser);
                        break;
                    }
                    Some(Command::EndAfterTurn(reason)) => {
                        // No turn to wait for.
                        self.close(&mut open, started, reason);
                        break;
                    }
                    // A turn cannot happen here; the harness reports none.
                    Some(Command::TurnDone { .. }) => {}
                    None => break,
                },
                _ = ticker.tick() => {
                    self.expire(&mut open, started);
                    if open.is_empty() && self.idle_elapsed() > self.idle_timeout {
                        self.close(&mut open, started, EndReason::Detached);
                        break;
                    }
                }
            }
        }
    }

    fn idle_elapsed(&self) -> Duration {
        self.last_seen
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }

    /// The gate already decided this needs the operator, so unlike the
    /// supervisor's path there is no policy to re-run: what arrives here has
    /// been asked for.
    fn on_permission(
        &self,
        open: &mut HashMap<String, oneshot::Sender<PermissionReply>>,
        started: Instant,
        request: PermissionRequest,
        reply: oneshot::Sender<PermissionReply>,
    ) {
        let row = permission_row(
            &self.session_id,
            &self.node_id,
            started.elapsed().as_millis() as i64,
            self.permission_timeout,
            &request,
        );
        if let Err(e) = self.store.insert_permission(&row) {
            tracing::error!(error = %e, "failed to record permission request");
            let _ = reply.send(PermissionReply::Selected(
                crate::acp::types::OPTION_REJECT_ONCE.into(),
            ));
            return;
        }
        let id = row.id.clone();
        open.insert(id.clone(), reply);
        self.record(
            ek::PERMISSION_REQUEST,
            Some(id.clone()),
            json!({
                "permission_id": id, "title": request.title, "kind": request.kind,
                "raw_input": request.raw_input, "options": request.options,
                "expires_ms": row.expires_ms
            }),
            started,
        );
        self.set_state(SessionState::WaitingOnYou, None, started);
    }

    /// Silence is a refusal here too.
    fn expire(
        &self,
        open: &mut HashMap<String, oneshot::Sender<PermissionReply>>,
        started: Instant,
    ) {
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
            return;
        }
        for id in due {
            if let Some(sender) = open.remove(&id) {
                let _ = sender.send(PermissionReply::Selected(
                    crate::acp::types::OPTION_REJECT_ONCE.into(),
                ));
            }
            let _ = self.store.resolve_permission(
                &id,
                "expired",
                None,
                started.elapsed().as_millis() as i64,
            );
            self.record(
                ek::PERMISSION_EXPIRED,
                Some(id.clone()),
                json!({ "permission_id": id, "reason": "denied: unanswered" }),
                started,
            );
        }
        self.back_to_running(open);
    }

    fn close(
        &self,
        open: &mut HashMap<String, oneshot::Sender<PermissionReply>>,
        started: Instant,
        reason: EndReason,
    ) {
        for (_, sender) in open.drain() {
            let _ = sender.send(PermissionReply::Cancelled);
        }
        self.set_state(SessionState::Closed, Some(reason), started);
    }

    fn back_to_running(&self, open: &HashMap<String, oneshot::Sender<PermissionReply>>) {
        if open.is_empty() {
            if let Ok(Some(s)) = self.store.get_session(&self.session_id) {
                if s.state == SessionState::WaitingOnYou.as_str() {
                    let _ = self.store.update_session(
                        &self.session_id,
                        SessionPatch::state(SessionState::Running.as_str()),
                    );
                    self.publish_session();
                }
            }
        }
        self.publish_queue();
    }

    fn set_state(&self, state: SessionState, end_reason: Option<EndReason>, started: Instant) {
        let mono = started.elapsed().as_millis() as i64;
        let patch = SessionPatch {
            state: Some(state.as_str().to_string()),
            end_reason: end_reason.map(|r| r.as_str().to_string()),
            ended_mono_ms: state.is_terminal().then_some(mono),
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
            started,
        );
        self.publish_session();
    }

    fn record(
        &self,
        kind: &str,
        ref_id: Option<String>,
        payload: serde_json::Value,
        started: Instant,
    ) {
        let e = NewEvent {
            session_id: self.session_id.clone(),
            work_item_id: None,
            kind: kind.to_string(),
            ref_id,
            payload,
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        };
        match self.store.append_event(&e) {
            Ok(seq) => self.bus.publish(Frame::Event {
                seq,
                node_id: self.node_id.clone(),
                session_id: e.session_id,
                kind: e.kind,
                ref_id: e.ref_id,
                payload: e.payload,
                at_ms: e.at_ms,
            }),
            Err(err) => tracing::error!(error = %err, "failed to persist event"),
        }
    }

    /// The session and the queue together, as the supervisor does it. A card
    /// created here changes both, and an interface already open learns about
    /// the queue only from the frame: without it the session says it is
    /// waiting on you while the thing to answer never appears.
    fn publish_session(&self) {
        if let Ok(Some(row)) = self.store.get_session(&self.session_id) {
            self.bus.publish(Frame::Session(Box::new(row)));
        }
        self.publish_queue();
    }

    fn publish_queue(&self) {
        if let Ok(open) = self.store.open_permissions() {
            self.bus.publish(Frame::Queue { waiting: open });
        }
    }
}
