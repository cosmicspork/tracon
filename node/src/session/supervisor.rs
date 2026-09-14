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
    stream::{Bus, Frame},
};

/// Output of a tool call is capped before it reaches the log; a single read can
/// carry an entire file.
const MAX_TOOL_OUTPUT: usize = 64 * 1024;

/// A harness turn must not hold a session open indefinitely. The timeout is
/// intentionally independent of a token budget: subscriptions need the same
/// runaway protection as metered providers.
const TURN_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
const PAUSE_QUIESCE_TIMEOUT: Duration = Duration::from_secs(30);
const WATCHDOG_FAILURE_LIMIT: u8 = 3;

/// How many times the same tool call has to arrive back to back before the
/// run is worth saying out loud. Only an unchanged call repeated with nothing
/// in between counts: a fix-then-test loop reruns the same command all day
/// with edits between the runs, and that is work, not a runaway.
const REPETITION_RUN: u32 = 3;

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
        /// The operator's rewrite of a brokered tool call's arguments.
        arguments: Option<serde_json::Value>,
        ack: oneshot::Sender<Result<(), String>>,
    },
    Pause {
        source: PauseSource,
        reason: String,
        ack: oneshot::Sender<Result<(), String>>,
    },
    Resume {
        source: PauseSource,
        reason: String,
        ack: oneshot::Sender<Result<(), String>>,
    },
    PauseQuiesceTimeout {
        turn_id: u64,
    },
    Kill,
    /// End the session once the running turn finishes (now, if none is):
    /// the work item closed, or the phase's artifact landed.
    EndAfterTurn(EndReason),
    /// A permission request that did not come from the harness: a brokered
    /// tool call the policy wants the operator to decide. Same queue, same
    /// expiry, same answer path.
    Permission {
        request: PermissionRequest,
        reply: oneshot::Sender<PermissionReply>,
    },
    /// Sent by a turn task when the harness finishes a turn. The supervisor
    /// records it rather than the task, so buffered text is flushed first and
    /// `turn_end` lands after the message it concludes.
    TurnDone {
        turn_id: u64,
        kind: &'static str,
        payload: serde_json::Value,
        tokens: i64,
    },
}

/// Why a pause occurred. The state transition is authoritative; this is
/// recorded so the operator can distinguish an intervention from a watchdog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseSource {
    Operator,
    Watchdog,
}

impl PauseSource {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Watchdog => "watchdog",
        }
    }
}

pub struct Supervisor {
    pub session_id: String,
    node_id: String,
    store: Arc<Store>,
    bus: Bus,
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
    policy: Arc<parking_lot::RwLock<crate::policy::Policy>>,
    channel: String,
    /// Set by `Command::EndAfterTurn` while a turn is running.
    end_after_turn: Option<EndReason>,
    /// The only completion allowed to update the current session. Clearing
    /// this fences a cancelled turn from resurrecting a paused or stopped row.
    active_turn: Option<u64>,
    next_turn: u64,
    /// The harness's running session cost when this turn was dispatched. What
    /// the harness reports is cumulative, so the turn's own share is the
    /// difference; `None` when the harness prices nothing.
    turn_start_cost_usd: Option<f64>,
    consecutive_failures: u8,
    /// The signature of the last tool call this turn, and how many times it
    /// has arrived unchanged in a row. Surfaced as a signal; never a reason
    /// to pause.
    last_tool_call: Option<String>,
    repeated_tool_calls: u32,
    /// A fenced turn is still allowed to report completion, but resume waits
    /// for that exact receipt rather than trusting cancel's enqueue ack.
    paused_turn: Option<u64>,
    /// The durable identity and history of this session's harness, for a
    /// harness that has one. It owns the sequence the stream resumes from, and
    /// it is what a mediated mutation's intent is written to before the
    /// mutation is dispatched.
    ingest: Option<Arc<crate::session::ingest::Ingest>>,
    /// The intent of the prompt currently in flight, settled when the turn
    /// reports and marked uncertain when it does not.
    prompt_intent: Option<String>,
}

impl Supervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: String,
        node_id: String,
        store: Arc<Store>,
        bus: Bus,
        handle: Arc<dyn HarnessHandle>,
        started: Instant,
        permission_timeout: Duration,
        self_tx: mpsc::Sender<Command>,
        runner: Arc<dyn Runner>,
        container: String,
        policy: Arc<parking_lot::RwLock<crate::policy::Policy>>,
        channel: String,
    ) -> Self {
        Self {
            policy,
            channel,
            end_after_turn: None,
            active_turn: None,
            next_turn: 0,
            turn_start_cost_usd: None,
            consecutive_failures: 0,
            last_tool_call: None,
            repeated_tool_calls: 0,
            paused_turn: None,
            ingest: None,
            prompt_intent: None,
            self_tx,
            runner,
            container,
            session_id,
            node_id,
            store,
            bus,
            handle,
            started,
            permission_timeout,
            open: Arc::new(Mutex::new(HashMap::new())),
            chunks: ChunkBuffer::default(),
        }
    }

    /// Give this session the ingestion layer that owns its harness's durable
    /// identity. Set for a harness whose stream is sequenced and replayable;
    /// a stdio harness has nothing to reconcile against and gets none.
    pub fn with_ingest(mut self, ingest: Arc<crate::session::ingest::Ingest>) -> Self {
        self.ingest = Some(ingest);
        self
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

    /// Move the session, unless it has already ended. An ending is final:
    /// whatever this task was in the middle of when a Stop landed — a
    /// permission arriving, a turn completing, a pause quiescing — must not
    /// write over it. The refusal is recorded rather than swallowed, and the
    /// row is still republished so a client sees the state that really holds.
    fn set_state(&self, state: SessionState, end_reason: Option<EndReason>) {
        let patch = SessionPatch {
            state: Some(state.as_str().to_string()),
            end_reason: end_reason.map(|r| r.as_str().to_string()),
            ended_mono_ms: state.is_terminal().then(|| self.mono_ms()),
            turn_active: state.is_terminal().then_some(false),
            ..Default::default()
        };
        match self
            .store
            .update_session_unless(&self.session_id, SessionState::TERMINAL, patch)
        {
            Ok(true) => {}
            Ok(false) => {
                // Ending a session that has already ended is this task
                // catching up with a Stop that wrote the row first, not
                // something trying to resurrect it: the outcome is the one
                // that is already recorded, and nothing is refused.
                if !state.is_terminal() {
                    self.refused("state", json!({ "attempted": state.as_str() }));
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
        );
        self.publish_session();
    }

    /// Write down that something arrived too late to be applied. `what` names
    /// the writer; the row's own state says what it was refused against.
    fn refused(&self, what: &str, mut detail: serde_json::Value) {
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
        self.record(ek::LATE_REFUSED, None, detail);
    }

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

    /// Drive the session until the harness exits or it is killed.
    pub async fn run(
        mut self,
        mut events: mpsc::Receiver<HarnessEvent>,
        mut commands: mpsc::Receiver<Command>,
    ) {
        let mut killed_by_us = false;
        // Stop can land between the startup handoff's last check and this
        // task's registration in `live`; only a row still `starting` may
        // become the one this task drives, so that race cannot resurrect a
        // row Stop already closed.
        if self.claim_running() {
            let mut ticker = tokio::time::interval(Duration::from_secs(5));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                        Some(Command::Answer { permission_id, option_id, arguments, ack }) => {
                            let _ = ack.send(self.on_answer(&permission_id, &option_id, arguments).await);
                        }
                        Some(Command::Pause { source, reason, ack }) => {
                            let _ = ack.send(self.pause(source, &reason).await);
                        }
                        Some(Command::Resume { source, reason, ack }) => {
                            let _ = ack.send(self.resume(source, &reason).await);
                        }
                        Some(Command::PauseQuiesceTimeout { turn_id }) => {
                            if self.paused_turn == Some(turn_id) && self.is_paused() {
                                killed_by_us = true;
                                self.shutdown(EndReason::Error).await;
                                break;
                            }
                        }
                        Some(Command::TurnDone { turn_id, kind, payload, tokens }) => {
                            if self.active_turn != Some(turn_id) {
                                self.refused(
                                    "turn_done",
                                    json!({ "late_completion_ignored": true, "turn_id": turn_id }),
                                );
                                continue;
                            }
                            let turn_timeout = payload["turn_timeout"] == true;
                            let paused_completion = self.paused_turn == Some(turn_id);
                            self.active_turn = None;
                            self.on_turn_done(kind, payload, tokens).await;
                            if paused_completion {
                                // The adapter's barrier drained its inbound reader
                                // before it let TurnDone through. Drain the
                                // supervisor queue too, while still fenced, so a
                                // stale permission/output cannot cross Resume.
                                while let Ok(event) = events.try_recv() {
                                    let _ = self.on_harness_event(event).await;
                                }
                                self.paused_turn = None;
                                continue;
                            }
                            if turn_timeout {
                                let reason = "watchdog stopped a session after its harness turn timed out";
                                self.set_state(SessionState::Paused, None);
                                self.record(
                                    ek::SESSION_PAUSED,
                                    None,
                                    json!({ "source": PauseSource::Watchdog.as_str(), "reason": reason }),
                                );
                                killed_by_us = true;
                                self.shutdown(EndReason::Error).await;
                                break;
                            }
                            if self.watchdog_pause_if_needed().await {
                                continue;
                            }
                            if self.check_budget().await {
                                break;
                            }
                            if let Some(reason) = self.end_after_turn.take() {
                                killed_by_us = true;
                                self.shutdown(reason).await;
                                break;
                            }
                        }
                        Some(Command::EndAfterTurn(reason)) => {
                            let turning = self
                                .store
                                .get_session(&self.session_id)
                                .ok()
                                .flatten()
                                .is_some_and(|s| s.turn_active != 0);
                            if turning {
                                self.end_after_turn = Some(reason);
                            } else {
                                killed_by_us = true;
                                self.shutdown(reason).await;
                                break;
                            }
                        }
                        Some(Command::Permission { request, reply }) => {
                            self.on_permission(request, reply).await;
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
        }

        if !killed_by_us {
            self.finish_unexpected().await;
        }
        self.flush_chunks();
        self.remove_container().await;
    }

    /// The only path from `starting` to `running`: guarded so a Stop that
    /// closed the row in the gap between the startup handoff's last check
    /// and this task's registration cannot be overwritten back to running.
    fn claim_running(&self) -> bool {
        match self.store.update_session_if(
            &self.session_id,
            SessionState::Starting.as_str(),
            SessionPatch::state(SessionState::Running.as_str()),
        ) {
            Ok(true) => {
                self.record(
                    ek::STATE,
                    None,
                    json!({ "state": "running", "end_reason": null }),
                );
                self.publish_session();
                true
            }
            // The handoff lost: the row is no longer `starting`, so a Stop
            // (or a previous claim) owns it. Say so, rather than leaving the
            // operator to infer a silently abandoned startup.
            other => {
                if let Err(e) = other {
                    tracing::error!(session = %self.session_id, error = %e, "failed to claim the session");
                }
                self.refused("startup_handoff", json!({ "attempted": "running" }));
                false
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
    fn is_fenced(&self) -> bool {
        self.store
            .get_session(&self.session_id)
            .ok()
            .flatten()
            .is_some_and(|s| {
                let state = SessionState::from_stored(&s.state);
                state == SessionState::Paused || state.is_terminal()
            })
    }
    fn is_terminal(&self) -> bool {
        self.store
            .get_session(&self.session_id)
            .ok()
            .flatten()
            .is_some_and(|s| SessionState::from_stored(&s.state).is_terminal())
    }

    async fn pause(&mut self, source: PauseSource, reason: &str) -> Result<(), String> {
        let row = self
            .store
            .get_session(&self.session_id)
            .map_err(|e| e.to_string())?
            .ok_or("session is gone")?;
        if SessionState::from_stored(&row.state).is_terminal() {
            return Err("session is terminal".into());
        }
        if row.state == SessionState::Paused.as_str() {
            return Ok(());
        }

        // Fence first. Cancel only queues an interrupt, so keep the active
        // turn identity and wait for its matching completion before resume.
        let active = self.active_turn;
        self.set_state(SessionState::Paused, None);
        self.record(
            ek::SESSION_PAUSED,
            None,
            json!({ "source": source.as_str(), "reason": reason }),
        );
        if source == PauseSource::Watchdog {
            let _ = self.store.update_session(
                &self.session_id,
                SessionPatch {
                    last_error: Some(reason.to_string()),
                    ..Default::default()
                },
            );
            self.publish_session();
        }
        self.reject_open_permissions().await;
        if let Some(turn_id) = active {
            self.paused_turn = Some(turn_id);
            if tokio::time::timeout(CANCEL_TIMEOUT, self.handle.cancel())
                .await
                .is_err()
            {
                self.shutdown(EndReason::Error).await;
                return Err("could not queue the interrupt; the session was stopped".into());
            }
            let tx = self.self_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(PAUSE_QUIESCE_TIMEOUT).await;
                let _ = tx.send(Command::PauseQuiesceTimeout { turn_id }).await;
            });
        }
        Ok(())
    }

    async fn resume(&mut self, source: PauseSource, reason: &str) -> Result<(), String> {
        let row = self
            .store
            .get_session(&self.session_id)
            .map_err(|e| e.to_string())?
            .ok_or("session is gone")?;
        if row.state != SessionState::Paused.as_str() {
            return Err(format!("session is {}", row.state));
        }
        if self.paused_turn.is_some() {
            return Err("waiting for the paused turn to finish".into());
        }
        self.consecutive_failures = 0;
        self.set_state(SessionState::Running, None);
        self.record(
            ek::SESSION_RESUMED,
            None,
            json!({ "source": source.as_str(), "reason": reason }),
        );
        Ok(())
    }

    async fn watchdog_pause_if_needed(&mut self) -> bool {
        if self.consecutive_failures < WATCHDOG_FAILURE_LIMIT || self.is_paused() {
            return false;
        }
        // Repeated failure is a signal the operator is owed, not a verdict on
        // how much progress was made, so the reason says what repeated and
        // what was done about it rather than reading as a scoreboard.
        let reason = format!(
            "the same failure repeated {} times in a row; paused on that signal so you can look",
            self.consecutive_failures
        );
        let _ = self.pause(PauseSource::Watchdog, &reason).await;
        true
    }

    /// Record a run of identical tool calls. This never pauses and never
    /// touches the session's state: a repeated call is evidence for the
    /// operator to read, and repetition on its own does not distinguish a
    /// stuck agent from an agent doing repetitive work. Only repeated
    /// failure, counted separately, is allowed to fence a session.
    fn note_repetition(&mut self, call: &crate::adapter::types::ToolCall) {
        let signature = json!({
            "title": call.title, "kind": call.kind, "raw_input": call.raw_input,
        })
        .to_string();
        if self.last_tool_call.as_deref() != Some(signature.as_str()) {
            self.last_tool_call = Some(signature);
            self.repeated_tool_calls = 1;
            return;
        }
        self.repeated_tool_calls = self.repeated_tool_calls.saturating_add(1);
        let count = self.repeated_tool_calls;
        // Said once when the run reaches the threshold, then once per further
        // run of the same length: a loop of fifty calls is one situation, not
        // forty-eight of them.
        if count < REPETITION_RUN || !count.is_multiple_of(REPETITION_RUN) {
            return;
        }
        self.record(
            ek::REPETITION,
            None,
            json!({
                "what": "tool_call",
                "count": count,
                "title": call.title,
                "kind": call.kind,
                "paused": false,
            }),
        );
    }

    async fn on_harness_event(&mut self, ev: HarnessEvent) -> bool {
        if self.is_fenced() {
            // A session that has ended is not merely quiet: work still
            // arriving for it is a late completion, and the log says so once
            // rather than absorbing it silently.
            let terminal = self.is_terminal();
            match ev {
                HarnessEvent::Permission { reply, .. } => {
                    if terminal {
                        self.refused("harness_permission", json!({}));
                    }
                    let _ = reply.send(PermissionReply::Cancelled);
                }
                HarnessEvent::Exited { code } => {
                    self.record(ek::ERROR, None, json!({ "harness_exit_code": code }));
                    return true;
                }
                _ => {
                    if terminal {
                        self.refused("harness_event", json!({}));
                    }
                }
            }
            return false;
        }
        match ev {
            HarnessEvent::MessageChunk { message_id, text } => {
                self.bus.publish(Frame::Chunk {
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
                self.bus.publish(Frame::Chunk {
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
                self.note_repetition(&t);
            }
            HarnessEvent::ToolCallUpdate(t) => {
                // Updates are cumulative; only the terminal one is worth keeping.
                self.bus.publish(Frame::ToolUpdate {
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
            HarnessEvent::Other(v) => {
                if let Some(retry) = retry_notice(&v) {
                    self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                    let attempt = retry.attempt.unwrap_or_else(|| {
                        self.store
                            .count_events_this_turn(&self.session_id, ek::PROVIDER_ERROR)
                            .unwrap_or(0)
                            + 1
                    });
                    self.record(
                        ek::PROVIDER_ERROR,
                        None,
                        json!({
                            "provider": retry.provider,
                            "status": retry.status,
                            "message": retry.message,
                            "attempt": attempt,
                            "source": "harness",
                        }),
                    );
                    let _ = self.watchdog_pause_if_needed().await;
                }
            }
            HarnessEvent::Models(_) => {}
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
        if self.is_fenced() {
            let _ = reply.send(PermissionReply::Cancelled);
            return;
        }
        let decision = self.policy.read().decide(&crate::policy::Request {
            channel: &self.channel,
            kind: request.kind.as_deref(),
            title: &request.title,
            command: command.as_deref(),
            arguments: None,
        });
        match decision.verdict {
            crate::policy::Verdict::Allow | crate::policy::Verdict::Deny => {
                let allow = decision.verdict == crate::policy::Verdict::Allow;
                let option = if allow {
                    crate::adapter::types::OPTION_ALLOW_ONCE
                } else {
                    crate::adapter::types::OPTION_REJECT_ONCE
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

        let row = permission_row(
            &self.session_id,
            &self.node_id,
            self.mono_ms(),
            self.permission_timeout,
            &request,
        );
        let id = row.id.clone();
        if let Err(e) = self.store.insert_permission(&row) {
            tracing::error!(error = %e, "failed to record permission request");
            let _ = reply.send(PermissionReply::Selected(
                crate::adapter::types::OPTION_REJECT_ONCE.into(),
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

    async fn on_answer(
        &mut self,
        permission_id: &str,
        option_id: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<(), String> {
        if self.is_fenced() {
            return Err("session is paused".into());
        }
        on_answer_row(
            &self.store,
            &mut *self.open.lock().await,
            permission_id,
            option_id,
            arguments.clone(),
            self.mono_ms(),
        )?;
        self.record(
            ek::PERMISSION_ANSWER,
            Some(permission_id.to_string()),
            json!({ "permission_id": permission_id, "option_id": option_id, "arguments": arguments }),
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
                    crate::adapter::types::OPTION_REJECT_ONCE.into(),
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
        if s.budget_tokens > 0 && s.tokens_used >= s.budget_tokens {
            return Err("session is over budget".into());
        }
        if s.state != SessionState::Running.as_str() {
            return Err(format!("session is {}", s.state));
        }
        // A prompt depends on knowing whether the last one landed. While that
        // is unknown, sending another could duplicate a turn that is already
        // running upstream, so it is refused with the reason rather than
        // guessed at. Reconciliation clears it; so can the operator.
        if let Some(ingest) = &self.ingest {
            if let Some(reason) = ingest.uncertain_reason() {
                return Err(format!(
                    "the outcome of the last mutation on this session is unknown ({reason}); \
                     prompting again could duplicate a turn the harness may already be running"
                ));
            }
        }
        self.next_turn = self.next_turn.wrapping_add(1);
        let turn_id = self.next_turn;
        self.active_turn = Some(turn_id);
        let _ = self.store.update_session(
            &self.session_id,
            SessionPatch {
                turn_active: Some(true),
                ..Default::default()
            },
        );
        // Sent is sent: the operator's box is empty from here, and a draft
        // save still in flight from the client must not resurrect it.
        let _ = self.store.set_draft(&self.session_id, None);
        // Open the turn's usage ledger before anything can spend against it.
        // The gateway attributes every model call to the session's current
        // turn, so the row has to exist first or the first call of the turn
        // would land on the previous one.
        let _ = self.store.begin_turn(&self.session_id);
        self.turn_start_cost_usd = self
            .store
            .get_session(&self.session_id)
            .ok()
            .flatten()
            .and_then(|s| s.cost_usd);
        // Written down before the dispatch, not after: the gap between the
        // two is exactly where an answer can be lost, and a row that says
        // "this may have happened" is what makes asking possible.
        self.prompt_intent = self.ingest.as_ref().map(|ingest| {
            ingest.intent_begin(crate::store::intent_kind::PROMPT, None, Some(text.as_str()))
        });
        self.record(ek::USER_PROMPT, None, json!({ "text": text }));
        self.publish_session();

        let handle = self.handle.clone();
        let done = self.self_tx.clone();
        // The supervisor owns the result: a stalled harness cannot leave a
        // permanently active turn, and a late answer carries this turn id.
        tokio::spawn(async move {
            let (kind, mut payload, tokens) =
                match tokio::time::timeout(TURN_TIMEOUT, handle.prompt(text)).await {
                    Ok(Ok(turn)) => (
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
                    Ok(Err(e)) => (ek::ERROR, json!({ "error": e.to_string() }), 0),
                    Err(_) => {
                        let cancel_confirmed = matches!(
                            tokio::time::timeout(CANCEL_TIMEOUT, handle.cancel()).await,
                            Ok(Ok(()))
                        );
                        (
                            ek::ERROR,
                            json!({
                                "error": "harness turn timed out",
                                "turn_timeout": true,
                                "cancel_confirmed": cancel_confirmed,
                            }),
                            0,
                        )
                    }
                };
            match tokio::time::timeout(CANCEL_TIMEOUT, handle.quiesce_events()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => payload["event_barrier_error"] = json!(error.to_string()),
                Err(_) => payload["event_barrier_error"] = json!("event barrier timed out"),
            }
            let _ = done
                .send(Command::TurnDone {
                    turn_id,
                    kind,
                    payload,
                    tokens,
                })
                .await;
        });
        Ok(())
    }

    /// Record the end of a turn: flush whatever text was still streaming, log
    /// the outcome, reconcile the two usage sources, and charge the session.
    async fn on_turn_done(&mut self, kind: &'static str, payload: serde_json::Value, tokens: i64) {
        self.flush_chunks();
        self.settle_prompt(kind, &payload);
        let session = self.store.get_session(&self.session_id).ok().flatten();
        let previous = session.as_ref().map(|s| s.tokens_used).unwrap_or(0);
        // A turn that errored reports nothing rather than zero: there is a
        // difference between a harness that said "no tokens" and one that
        // never got to say anything, and only the first can agree with the
        // gateway.
        let harness_tokens = (kind != ek::ERROR).then_some(tokens);
        let cost = match (
            session.as_ref().and_then(|s| s.cost_usd),
            self.turn_start_cost_usd,
        ) {
            (Some(now), Some(before)) => Some((now - before).max(0.0)),
            (Some(now), None) => Some(now),
            _ => None,
        };
        self.turn_start_cost_usd = None;
        let usage =
            crate::metrics::settle_turn(&self.store, &self.session_id, harness_tokens, cost);
        let _ = self.store.update_session(
            &self.session_id,
            SessionPatch {
                turn_active: Some(false),
                tokens_used: Some(previous + usage.charged),
                ..Default::default()
            },
        );
        if kind == ek::ERROR {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        } else {
            self.consecutive_failures = 0;
        }
        // A run of identical calls is only meaningful inside the turn that
        // made it; the next turn starts from a different prompt.
        self.last_tool_call = None;
        self.repeated_tool_calls = 0;
        self.record(kind, None, payload);
        // After the turn it is about, not before: a note on what a turn spent
        // reads as nonsense above the line saying the turn ended.
        if let Some(usage_kind) = usage.event_kind() {
            self.record(usage_kind, None, usage.detail());
        }
        self.publish_session();
    }
    /// What became of the prompt this turn was dispatched with.
    ///
    /// A turn that ended is proof the harness took it. A turn that timed out
    /// is proof too — it ran long enough to be stopped. Anything else is a
    /// dispatch that never reported: the POST may have been received and acted
    /// on with only the answer lost, so the honest state is neither sent nor
    /// unsent, and the session refuses a new prompt until reconciliation asks
    /// the harness which it was.
    fn settle_prompt(&mut self, kind: &'static str, payload: &serde_json::Value) {
        let (Some(ingest), Some(intent)) = (self.ingest.as_ref(), self.prompt_intent.take()) else {
            return;
        };
        if kind != ek::ERROR || payload["turn_timeout"] == true {
            ingest.intent_settle(&intent, crate::store::intent_state::ADMITTED, None);
            return;
        }
        let reason = payload["error"]
            .as_str()
            .unwrap_or("the prompt dispatch did not report an outcome");
        ingest.mark_uncertain(&intent, reason);
    }

    /// Budget is checked at turn end: the harness reports usage per turn, so a
    /// single long turn can overshoot. Enforced by ending the session.
    async fn check_budget(&mut self) -> bool {
        let Ok(Some(s)) = self.store.get_session(&self.session_id) else {
            return false;
        };
        if s.budget_tokens > 0
            && s.tokens_used >= s.budget_tokens
            && !SessionState::from_stored(&s.state).is_terminal()
        {
            self.shutdown(EndReason::Budget).await;
            return true;
        }
        false
    }
    async fn remove_container(&self) {
        match tokio::time::timeout(CANCEL_TIMEOUT, self.runner.kill(&self.container)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(container = %self.container, error = %e, "could not remove harness container")
            }
            Err(_) => {
                tracing::warn!(container = %self.container, "timed out removing harness container")
            }
        }
    }

    async fn shutdown(&mut self, reason: EndReason) {
        let state = match reason {
            EndReason::Budget => SessionState::KilledBudget,
            EndReason::Error => SessionState::Failed,
            _ => SessionState::Closed,
        };
        // Fence first. A turn or model request that wakes after this point
        // cannot move a terminal session back into active work.
        self.active_turn = None;
        self.set_state(state, Some(reason));
        let _ = tokio::time::timeout(CANCEL_TIMEOUT, self.handle.cancel()).await;
        let _ = tokio::time::timeout(CANCEL_TIMEOUT, self.handle.close()).await;
        self.remove_container().await;
        self.reject_open_permissions().await;
    }

    async fn finish_unexpected(&mut self) {
        let Ok(Some(s)) = self.store.get_session(&self.session_id) else {
            return;
        };
        if SessionState::from_stored(&s.state).is_terminal() {
            return;
        }
        self.reject_open_permissions().await;
        if s.budget_tokens > 0 && s.tokens_used >= s.budget_tokens {
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
            "paused" => Self::Paused,
            "waiting_on_you" => Self::WaitingOnYou,
            "waiting_on_check" => Self::WaitingOnCheck,
            "killed_budget" => Self::KilledBudget,
            "failed" => Self::Failed,
            _ => Self::Closed,
        }
    }
}

/// What a harness said when it told the node it is retrying a provider call.
/// Every field is optional: the point of the notice is that it happened, and
/// the gateway records the authoritative status for calls it forwards.
#[derive(Debug, PartialEq)]
struct RetryNotice {
    provider: Option<String>,
    status: Option<i64>,
    message: Option<String>,
    attempt: Option<i64>,
}

/// Recognise a harness's own "the provider refused, I am retrying" notice.
///
/// Two harnesses say it two ways and neither is in a schema the node shares:
/// the Claude adapter forwards `{"type":"system","subtype":"api_retry",…}`,
/// and the OpenCode adapter forwards its `session.next.retried` event as
/// `{"method":"session.next.retried","params":…}`. Rather than model either,
/// match on the discriminator wherever it sits and read whatever fields came
/// with it. The stem is `retr` plus an ending, because the two harnesses do
/// not even agree on the tense.
fn retry_notice(v: &serde_json::Value) -> Option<RetryNotice> {
    let named_retry = [v["subtype"].as_str(), v["method"].as_str()]
        .into_iter()
        .flatten()
        .any(|name| name.contains("retry") || name.contains("retried"));
    if !named_retry {
        return None;
    }
    // The fields may sit on the notice itself or in whatever it wraps.
    let scopes = [v, &v["params"], &v["error"], &v["params"]["error"]];
    let string = |keys: &[&str]| {
        scopes.iter().find_map(|scope| {
            keys.iter()
                .find_map(|k| scope[*k].as_str())
                .map(str::to_string)
        })
    };
    let number = |keys: &[&str]| {
        scopes
            .iter()
            .find_map(|scope| keys.iter().find_map(|k| scope[*k].as_i64()))
    };
    Some(RetryNotice {
        provider: string(&["provider", "providerId", "providerID"]),
        status: number(&["status", "statusCode", "status_code", "code"]),
        message: string(&["message", "reason", "error"]),
        attempt: number(&["attempt", "attempts", "retry"]).filter(|n| *n > 0),
    })
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

/// The row a request becomes on the queue. Shared with the external
/// attachment loop, which asks the same way without a harness behind it.
pub(super) fn permission_row(
    session_id: &str,
    node_id: &str,
    mono_ms: i64,
    timeout: Duration,
    request: &PermissionRequest,
) -> PermissionRow {
    PermissionRow {
        id: uuid::Uuid::now_v7().to_string(),
        session_id: session_id.to_string(),
        node_id: node_id.to_string(),
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
        created_mono_ms: mono_ms,
        resolved_mono_ms: None,
        expires_ms: now_ms() + timeout.as_millis() as i64,
    }
}

/// Resolve an open request with the operator's answer and hand it to whoever
/// is waiting. The caller records the event and republishes the session.
/// Edited arguments are accepted only on a brokered tool call: the node runs
/// that call itself, while a harness's own request would carry the edit back
/// to a harness that never asked for it.
pub(super) fn on_answer_row(
    store: &Store,
    open: &mut HashMap<String, oneshot::Sender<PermissionReply>>,
    permission_id: &str,
    option_id: &str,
    arguments: Option<serde_json::Value>,
    mono_ms: i64,
) -> Result<(), String> {
    if let Some(args) = &arguments {
        let tool = store
            .get_permission(permission_id)
            .map_err(|e| e.to_string())?
            .is_some_and(|p| p.kind.as_deref() == Some(crate::mcp::TOOL_KIND));
        if !tool {
            return Err("only a brokered tool call can be answered with edited arguments".into());
        }
        if !args.is_object() {
            return Err("edited arguments must be an object".into());
        }
    }
    let Some(sender) = open.remove(permission_id) else {
        return Err("no open permission request with that id".into());
    };
    if !store
        .resolve_permission(permission_id, "answered", Some(option_id), mono_ms)
        .map_err(|e| e.to_string())?
    {
        return Err("permission request is no longer open".into());
    }
    let reply = match arguments {
        Some(arguments) if option_id == crate::adapter::types::OPTION_ALLOW_ONCE => {
            PermissionReply::Edited {
                option_id: option_id.to_string(),
                arguments,
            }
        }
        _ => PermissionReply::Selected(option_id.to_string()),
    };
    let _ = sender.send(reply);
    Ok(())
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
    fn a_retry_notice_is_recognised_whichever_harness_sent_it() {
        // The Claude adapter's system message.
        let claude = retry_notice(&json!({
            "type": "system", "subtype": "api_retry", "provider": "anthropic",
            "status": 429, "message": "Error", "attempt": 2
        }))
        .expect("claude's api_retry");
        assert_eq!(
            claude,
            RetryNotice {
                provider: Some("anthropic".into()),
                status: Some(429),
                message: Some("Error".into()),
                attempt: Some(2),
            }
        );
        // OpenCode's `session.next.retried`, as its adapter forwards it: past
        // tense, and the fields in the event's own payload rather than beside
        // the discriminator.
        let opencode = retry_notice(&json!({
            "method": "session.next.retried",
            "params": {
                "providerID": "openai-codex",
                "error": { "statusCode": 429, "message": "rate limited" },
            },
            "message": "rate limited",
        }))
        .expect("opencode's session.next.retried");
        assert_eq!(opencode.provider.as_deref(), Some("openai-codex"));
        assert_eq!(opencode.status, Some(429));
        assert_eq!(opencode.message.as_deref(), Some("rate limited"));
        assert_eq!(
            opencode.attempt, None,
            "the node counts when the harness does not"
        );
        // Anything else is not a retry, including a plain error.
        assert!(retry_notice(&json!({ "type": "system", "subtype": "init" })).is_none());
        assert!(retry_notice(&json!({ "method": "session.next.error" })).is_none());
        assert!(retry_notice(&json!({ "method": "session.updated" })).is_none());
    }

    #[test]
    fn unknown_state_strings_read_as_closed() {
        assert_eq!(SessionState::from_stored("running"), SessionState::Running);
        assert_eq!(SessionState::from_stored("nonsense"), SessionState::Closed);
    }
}
