//! A harness the operator runs themselves, attached to a channel.
//!
//! There is no process here to supervise: the harness is a terminal the
//! operator started, outside the boundary, and it reaches the node through the
//! operator door rather than the gateway's forward. What it still needs is a
//! session: the log of what was asked for belongs somewhere the operator can
//! read it afterwards, and a pause fences it. A brokered call the operator
//! must decide is held as an approval the channel owns, not a card this loop
//! waits on, so nothing here blocks on the operator.
//!
//! So each client on a channel gets one attached session and a loop that
//! handles only what an attachment can produce: a pause, a resume, and the
//! end. No turns, no budget, no container.

use std::{sync::Arc, time::Duration, time::Instant};

use serde_json::json;
use tokio::sync::mpsc;

use crate::{
    adapter::PermissionReply,
    session::{
        state::{event_kind as ek, EndReason, SessionState},
        supervisor::{Command, PauseSource},
    },
    store::{now_ms, NewEvent, SessionPatch, Store},
    stream::{Bus, Frame},
};

/// The `harness_id` an attached session records. Not an adapter: nothing is
/// launched, and `adapter_for` never sees it. The interface keys its wording
/// off this, and `recent_repos` filters on it.
pub const HARNESS_ID: &str = "external";

/// A channel and the `Mcp-Session-Id` its client echoes, if it echoes one.
pub(super) type Key = (String, Option<String>);

/// One attachment per client on a channel, while it is live.
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
    pub idle_timeout: Duration,
    pub last_seen: Arc<std::sync::Mutex<Instant>>,
}

impl Loop {
    /// Until the attachment is killed, goes quiet, or the node stops.
    pub(super) async fn run(self, mut commands: mpsc::Receiver<Command>) {
        let started = Instant::now();
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    // Nothing asks through an attachment any more: a call the
                    // operator decides is an approval. Refuse rather than
                    // leave a caller waiting on a card nobody will raise.
                    Some(Command::Permission { reply, .. }) => {
                        let _ = reply.send(PermissionReply::Cancelled);
                    }
                    Some(Command::Answer { ack, .. }) => {
                        let _ = ack.send(Err(
                            "nothing is waiting on this attachment; approvals are answered on their own"
                                .into(),
                        ));
                    }
                    Some(Command::Prompt { ack, .. }) => {
                        // The operator is already talking to this harness; the
                        // node is not in that conversation.
                        let _ = ack.send(Err(
                            "this session is a harness you run yourself; prompt it in its own terminal"
                                .into(),
                        ));
                    }
                    Some(Command::Pause { source, reason, ack }) => {
                        self.pause(started, source, &reason);
                        let _ = ack.send(Ok(()));
                    }
                    Some(Command::Resume { source, reason, ack }) => {
                        let done = self.resume(started, source, &reason);
                        let _ = ack.send(done);
                    }
                    Some(Command::Kill) => {
                        self.set_state(SessionState::Closed, Some(EndReason::KilledUser), started);
                        break;
                    }
                    Some(Command::EndAfterTurn(reason)) => {
                        // No turn to wait for.
                        self.set_state(SessionState::Closed, Some(reason), started);
                        break;
                    }
                    // A turn cannot happen here; the harness reports none.
                    Some(Command::TurnDone { .. }) | Some(Command::PauseQuiesceTimeout { .. }) => {}
                    None => break,
                },
                _ = ticker.tick() => {
                    if !self.is_paused() && self.idle_elapsed() > self.idle_timeout {
                        self.set_state(SessionState::Closed, Some(EndReason::Detached), started);
                        break;
                    }
                }
            }
        }
    }

    fn is_paused(&self) -> bool {
        self.store
            .get_session(&self.session_id)
            .ok()
            .flatten()
            .is_some_and(|s| s.state == SessionState::Paused.as_str())
    }

    /// There is no process to suspend outside the boundary. Pausing an
    /// attachment fences its broker access and makes that explicit to the
    /// external harness rather than pretending we stopped its terminal.
    fn pause(&self, started: Instant, source: PauseSource, reason: &str) {
        if self.is_paused() {
            return;
        }
        self.set_state(SessionState::Paused, None, started);
        self.record(
            ek::SESSION_PAUSED,
            None,
            json!({ "source": source.as_str(), "reason": reason }),
            started,
        );
        self.publish_queue();
    }

    fn resume(&self, started: Instant, source: PauseSource, reason: &str) -> Result<(), String> {
        if !self.is_paused() {
            return Err("external harness broker access is not paused".into());
        }
        self.set_state(SessionState::Running, None, started);
        self.record(
            ek::SESSION_RESUMED,
            None,
            json!({ "source": source.as_str(), "reason": reason }),
            started,
        );
        // A pause is this row's state now; the channel-wide flag is only
        // what an older node left behind, and a resume retires it.
        self.clear_legacy_pause();
        Ok(())
    }

    fn clear_legacy_pause(&self) {
        let Ok(Some(session)) = self.store.get_session(&self.session_id) else {
            return;
        };
        let Some(channel) = self.store.channel_get(&session.channel).ok().flatten() else {
            return;
        };
        let mut bindings: serde_json::Value =
            serde_json::from_str(&channel.bindings_json).unwrap_or_else(|_| json!({}));
        if bindings["external_paused"] != true {
            return;
        }
        let keyring = channel.keyring;
        bindings["external_paused"] = json!(false);
        let _ = self.store.channel_put(
            &session.channel,
            &keyring,
            &serde_json::to_string(&bindings).unwrap_or_else(|_| "{}".into()),
        );
    }

    fn idle_elapsed(&self) -> Duration {
        self.last_seen
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }

    /// The attachment's own transition writer, fenced exactly as the
    /// supervisor's is: a call still in flight when the operator stopped the
    /// attachment must not put the row back on the queue.
    fn set_state(&self, state: SessionState, end_reason: Option<EndReason>, started: Instant) {
        let mono = started.elapsed().as_millis() as i64;
        let patch = SessionPatch {
            state: Some(state.as_str().to_string()),
            end_reason: end_reason.map(|r| r.as_str().to_string()),
            ended_mono_ms: state.is_terminal().then_some(mono),
            turn_active: (state.is_terminal() || state == SessionState::Paused).then_some(false),
            ..Default::default()
        };
        match self
            .store
            .update_session_unless(&self.session_id, SessionState::TERMINAL, patch)
        {
            Ok(true) => {}
            Ok(false) => {
                // Closing an attachment that is already closed is this loop
                // catching up with a Stop, not a resurrection.
                if !state.is_terminal() {
                    self.refused("state", json!({ "attempted": state.as_str() }), started);
                }
                self.publish_session();
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to update session state");
                return;
            }
        }
        self.record(
            ek::STATE,
            None,
            json!({ "state": state.as_str(), "end_reason": end_reason.map(|r| r.as_str()) }),
            started,
        );
        self.publish_session();
    }

    /// Write down a transition that arrived after the attachment ended.
    fn refused(&self, what: &str, mut detail: serde_json::Value, started: Instant) {
        let state = self
            .store
            .get_session(&self.session_id)
            .ok()
            .flatten()
            .map(|row| row.state)
            .unwrap_or_else(|| "gone".into());
        detail["what"] = json!(what);
        detail["state"] = json!(state);
        tracing::warn!(session = %self.session_id, %what, %state, "refused a late transition");
        self.record(ek::LATE_REFUSED, None, detail, started);
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
        if let Ok(open) = self.store.open_permission_views() {
            self.bus.publish(Frame::Queue { waiting: open });
        }
    }
}
