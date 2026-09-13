//! Ingestion: the node's side of an OpenCode session's identity and history.
//!
//! The adapter reads OpenCode's streams and turns them into harness events;
//! this decides what those events *mean* for a session that has to survive a
//! dropped connection, a restarted node, and a mutation whose answer never
//! came back. Three things live here and nowhere else.
//!
//! **The sequence.** OpenCode's per-session stream is the only one with
//! replay, anchored on a durable aggregate sequence (finding 6). The adapter
//! asks for `?after=<seq>`; the *number* belongs here, in the store, because a
//! number kept in a process's memory is exactly the thing a restart loses. So
//! ingestion is keyed on (session, seq): admitting a sequence is a conditional
//! UPDATE that succeeds once, and every path — the live stream, a reconnect's
//! overlap, a snapshot read at startup — goes through it. A re-delivered event
//! produces no second tracon event because it is never admitted a second time.
//!
//! **The identities.** `ses_`, `msg_`, `prt_`, `per_`, `que_`, `pty_` are
//! OpenCode's names; the session, event, and permission ids are the node's.
//! The map between them is durable, so a permission re-raised after a
//! reconnect is recognised as the same request rather than asked twice. A tool
//! call has no upstream id at all — its `callID` is provider-supplied text —
//! so it is keyed by `<assistantMessageID>/<callID>` and mapped, never adopted.
//!
//! **The uncertainty.** A prompt whose POST times out may or may not have been
//! admitted. Re-sending it would duplicate a turn's work, and reporting it as
//! failed would be a lie. So the intent is written down *before* dispatch, the
//! session is marked `uncertain`, dependent operations are refused, and
//! reconciliation asks the harness what actually happened rather than acting
//! on a guess. A permission reply is the one mutation that is idempotent, so
//! that one is re-sent rather than asked about.
//!
//! What is deliberately *not* here: driving a session OpenCode made for
//! itself. A `session.created` naming a parent is recorded with its lineage
//! and surfaced as `untracked`, because there is no route to create a child
//! through tracon yet and an untracked worker the node cannot supervise is
//! worse than a visible refusal.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::adapter::{DurableCursor, HarnessSnapshots, PermissionReply, PermissionRequest};
use crate::session::state::{event_kind as ek, EndReason, SessionState};
use crate::session::supervisor::Command;
use crate::store::{intent_kind, intent_state, now_ms, object_kind, NewEvent, SessionPatch, Store};
use crate::stream::{Bus, Frame};

/// One page of `history?after=` is capped upstream at 100 (`api-ui.md` §3).
const HISTORY_PAGE: u64 = 100;
/// A bound on how far a startup reconciliation will page. A session with more
/// than this outstanding is one nobody is going to read event by event anyway;
/// the cap keeps a restart from turning into an unbounded read.
const HISTORY_PAGES: usize = 50;

/// Why reconciliation is running, which decides how much of it applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconcile {
    /// The node started and found a session that may still be alive upstream.
    /// Nothing is streaming, so the durable history is read here.
    Startup,
    /// The stream dropped and is about to be reopened. What was missed comes
    /// back through the resumed stream itself — that is what `?after=` is for
    /// — so history is *not* re-read; only what a stream cannot carry is.
    Reconnect,
}

/// What one reconciliation did. Returned so a caller (and a test) can see the
/// decisions rather than infer them from the log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The harness no longer has this session; it was moved to a terminal
    /// state with the reason.
    pub gone: bool,
    /// Durable events read from the snapshot and ingested.
    pub ingested: usize,
    /// Permissions pending upstream and unanswered here, put back on the queue.
    pub reraised: usize,
    /// Permissions answered here and still pending upstream, answered again.
    pub resent: usize,
    /// A turn that ended while the node was disconnected, closed with its usage.
    pub turn_closed: bool,
    /// Mutations whose outcome was established, clearing the session's doubt.
    pub settled: usize,
}

/// The durable identity and history of one OpenCode session.
pub struct Ingest {
    store: Arc<Store>,
    bus: Bus,
    node_id: String,
    session_id: String,
    started: Instant,
    /// The supervisor's own command channel, so a permission re-raised by
    /// reconciliation goes through the queue, the policy and the expiry every
    /// other request goes through rather than a second path beside them.
    commands: mpsc::Sender<Command>,
    upstream: Mutex<Option<String>>,
    api: Mutex<Option<Arc<dyn HarnessSnapshots>>>,
}

impl Ingest {
    pub fn new(
        store: Arc<Store>,
        bus: Bus,
        node_id: String,
        session_id: String,
        started: Instant,
        commands: mpsc::Sender<Command>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            bus,
            node_id,
            session_id,
            started,
            commands,
            upstream: Mutex::new(None),
            api: Mutex::new(None),
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn upstream_id(&self) -> Option<String> {
        self.upstream.lock().unwrap().clone()
    }

    /// Tie this session to an upstream one without a live harness behind it:
    /// what a startup reconciliation does before it has anything to stream.
    pub fn rebind(&self, upstream_id: &str, api: Arc<dyn HarnessSnapshots>) {
        let _ = self
            .store
            .opencode_bind(&self.session_id, upstream_id, None);
        *self.upstream.lock().unwrap() = Some(upstream_id.to_string());
        *self.api.lock().unwrap() = Some(api);
    }

    fn mono_ms(&self) -> i64 {
        self.started.elapsed().as_millis() as i64
    }

    /// Persist an event and publish it, exactly as the supervisor does: an
    /// event ingestion writes is an event on the same log, with the same
    /// sequence as its own id, not a second channel of its own.
    fn record(&self, kind: &str, ref_id: Option<String>, payload: Value) -> Option<i64> {
        let event = NewEvent {
            session_id: self.session_id.clone(),
            work_item_id: None,
            kind: kind.to_string(),
            ref_id,
            payload,
            at_ms: now_ms(),
            mono_ms: self.mono_ms(),
        };
        match self.store.append_event(&event) {
            Ok(seq) => {
                self.bus.publish(Frame::Event {
                    seq,
                    node_id: self.node_id.clone(),
                    session_id: event.session_id,
                    kind: event.kind,
                    ref_id: event.ref_id,
                    payload: event.payload,
                    at_ms: event.at_ms,
                });
                Some(seq)
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to persist an ingested event");
                None
            }
        }
    }

    fn publish_session(&self) {
        if let Ok(Some(row)) = self.store.get_session(&self.session_id) {
            self.bus.publish(Frame::Session(Box::new(row)));
        }
    }

    // ---- uncertainty ------------------------------------------------------

    /// Whether a mediated mutation's outcome is unknown. A prompt is refused
    /// while this holds: a second one would duplicate a turn that may already
    /// be running.
    pub fn is_uncertain(&self) -> bool {
        self.store
            .opencode_session_of(&self.session_id)
            .ok()
            .flatten()
            .is_some_and(|row| row.is_uncertain())
    }

    /// Why, for the refusal the operator reads.
    pub fn uncertain_reason(&self) -> Option<String> {
        self.store
            .opencode_session_of(&self.session_id)
            .ok()
            .flatten()
            .filter(|row| row.is_uncertain())
            .and_then(|row| row.uncertain_reason)
    }

    /// Write down what a mutation is about to do, before it is dispatched.
    /// Returns the intent's id, which the caller settles when it learns the
    /// outcome — or does not, which is itself the record that it never did.
    pub fn intent_begin(&self, kind: &str, target: Option<&str>, detail: Option<&str>) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        if let Err(e) =
            self.store
                .opencode_intent_begin(&id, &self.session_id, kind, target, detail)
        {
            tracing::error!(error = %e, "failed to record a mutation's intent");
        }
        id
    }

    pub fn intent_settle(&self, id: &str, state: &str, note: Option<&str>) {
        let _ = self.store.opencode_intent_settle(id, state, note);
    }

    /// The dispatch did not report an outcome. Neither sent nor not-sent: the
    /// session is marked, the reason is visible, and dependent operations are
    /// refused until reconciliation establishes what happened.
    pub fn mark_uncertain(&self, intent_id: &str, reason: &str) {
        self.intent_settle(intent_id, intent_state::UNCERTAIN, Some(reason));
        let _ = self.store.opencode_set_uncertain(&self.session_id, reason);
        self.record(
            ek::UNCERTAIN,
            None,
            json!({ "reason": reason, "intent": intent_id, "refusing": "prompt" }),
        );
        self.publish_session();
    }

    fn clear_uncertain(&self, note: &str) {
        if self
            .store
            .opencode_clear_uncertain(&self.session_id)
            .unwrap_or(false)
        {
            self.record(ek::UNCERTAIN, None, json!({ "cleared": note }));
            self.publish_session();
        }
    }

    // ---- identity ---------------------------------------------------------

    /// Record what this event names, whether or not it is admitted: a replay
    /// confirms the map rather than rewriting it, and a child session named on
    /// an event the node has already seen is still a child session.
    fn map_identities(&self, event: &Value, seq: i64) {
        let data = &event["data"];
        let kind = event["type"].as_str().unwrap_or_default();
        self.note_child(kind, data);

        let map = |object: &str, id: &str, ref_id: Option<&str>| {
            let _ = self
                .store
                .opencode_map(&self.session_id, object, id, ref_id, None, seq);
        };
        for candidate in [
            data["assistantMessageID"].as_str(),
            data["messageID"].as_str(),
            data["partID"].as_str(),
            data["ptyID"].as_str(),
            data["requestID"].as_str(),
            data["id"].as_str(),
            data["info"]["id"].as_str(),
        ]
        .into_iter()
        .flatten()
        {
            match object_of(candidate) {
                Some(object) => map(object, candidate, None),
                None => continue,
            }
        }
        // A tool call has no id of its own: `callID` is the provider's string.
        // Key it by the assistant message it belongs to, so two providers that
        // both say `call_1` in the same session cannot collide.
        if let Some(call) = data["callID"].as_str().filter(|c| !c.is_empty()) {
            let message = data["assistantMessageID"]
                .as_str()
                .or_else(|| data["messageID"].as_str())
                .unwrap_or("");
            map(
                object_kind::TOOL_CALL,
                &format!("{message}/{call}"),
                Some(call),
            );
        }
    }

    /// A session OpenCode made for itself. Recorded with its parent and
    /// surfaced once; never driven. The plan's position is that an untracked
    /// worker is refused visibly rather than adopted, and until there is a
    /// route to create a child through tracon, recording it and saying so is
    /// the honest half of that.
    fn note_child(&self, kind: &str, data: &Value) {
        if !kind.contains("session.created") && !kind.contains("session.updated") {
            return;
        }
        let child = data["info"]["id"]
            .as_str()
            .or_else(|| data["sessionID"].as_str())
            .unwrap_or_default();
        let parent = data["info"]["parentID"]
            .as_str()
            .or_else(|| data["parentID"].as_str())
            .unwrap_or_default();
        if child.is_empty() || parent.is_empty() || child == parent {
            return;
        }
        if self.upstream_id().as_deref() == Some(child) {
            return;
        }
        if self
            .store
            .opencode_record_child(child, parent)
            .unwrap_or(false)
        {
            self.record(
                ek::CHILD_SESSION,
                Some(child.to_string()),
                json!({
                    "child": child,
                    "parent": parent,
                    "untracked": true,
                    "reason": "the harness created a session tracon did not: it is recorded \
                               with its lineage and left undriven, because there is no route \
                               to register a child session yet",
                }),
            );
            self.publish_session();
        }
    }

    // ---- ingestion --------------------------------------------------------

    /// Claim one durable sequence. True exactly once per (session, seq).
    fn admit_seq(&self, seq: u64) -> bool {
        self.store
            .opencode_admit_seq(&self.session_id, seq as i64)
            .unwrap_or(false)
    }

    /// Turn one durable event into the tracon events it stands for. Used only
    /// on the snapshot path: while a stream is running, the adapter is what
    /// translates, and both go through `admit_seq` so neither can double the
    /// other.
    fn ingest_snapshot_event(&self, event: &Value) -> bool {
        let Some(seq) = event["durable"]["seq"].as_u64() else {
            return false;
        };
        self.map_identities(event, seq as i64);
        if !self.admit_seq(seq) {
            return false;
        }
        let kind = event["type"].as_str().unwrap_or_default();
        let data = &event["data"];
        let message = data["assistantMessageID"].as_str().map(str::to_string);
        match kind {
            "session.next.text.ended" => {
                let text = data["text"].as_str().unwrap_or_default();
                if !text.is_empty() {
                    self.record(ek::MESSAGE, message, json!({ "text": text }));
                }
            }
            "session.next.reasoning.ended" => {
                let text = data["text"].as_str().unwrap_or_default();
                if !text.is_empty() {
                    self.record(ek::THOUGHT, message, json!({ "text": text }));
                }
            }
            "session.next.tool.called" => {
                self.record(
                    ek::TOOL_CALL,
                    data["callID"].as_str().map(str::to_string),
                    json!({
                        "title": data["tool"].as_str().unwrap_or("a tool"),
                        "kind": "other",
                        "status": "in_progress",
                        "raw_input": data["input"].clone(),
                        "source": "reconciled",
                    }),
                );
            }
            "session.next.tool.success" | "session.next.tool.failed" => {
                let failed = kind.ends_with("failed");
                self.record(
                    ek::TOOL_RESULT,
                    data["callID"].as_str().map(str::to_string),
                    json!({
                        "status": if failed { "failed" } else { "completed" },
                        "output": if failed { data["error"].clone() } else { data["structured"].clone() },
                        "truncated": false,
                        "source": "reconciled",
                    }),
                );
            }
            "session.next.step.ended" => {
                let usage = usage_of(&data["tokens"]);
                self.record(
                    ek::USAGE,
                    None,
                    json!({ "used": usage, "source": "reconciled" }),
                );
            }
            _ => {}
        }
        true
    }

    // ---- reconciliation ---------------------------------------------------

    fn api(&self) -> Option<Arc<dyn HarnessSnapshots>> {
        self.api.lock().unwrap().clone()
    }

    /// Bring the node's record of this session back into agreement with the
    /// harness's. Safe to run repeatedly: everything it does is keyed on the
    /// durable sequence or on an identity already mapped.
    pub async fn reconcile(&self, mode: Reconcile) -> Report {
        let mut report = Report::default();
        let (Some(api), Some(upstream)) = (self.api(), self.upstream_id()) else {
            return report;
        };
        if self.is_terminal() {
            return report;
        }

        // Does the harness still have it? Everything else is only meaningful
        // if it does.
        match api.get(&format!("/api/session/{upstream}")).await {
            Ok(found) if found["data"]["id"].as_str().is_some() => {}
            Ok(_) => {
                self.gone("the harness no longer has this session");
                report.gone = true;
                return report;
            }
            Err(e) => {
                let text = e.to_string();
                if text.contains("404") || text.contains("410") {
                    self.gone(&format!("the harness no longer has this session: {text}"));
                    report.gone = true;
                }
                // Any other failure is the harness being unreachable, which is
                // not evidence that the session is gone. Say nothing.
                return report;
            }
        }

        if mode == Reconcile::Startup {
            self.replay_history(api.as_ref(), &upstream, &mut report)
                .await;
        }
        self.settle_intents(api.as_ref(), &upstream, &mut report)
            .await;
        self.reconcile_permissions(api.as_ref(), &upstream, &mut report)
            .await;
        report
    }

    fn is_terminal(&self) -> bool {
        self.store
            .get_session(&self.session_id)
            .ok()
            .flatten()
            .is_some_and(|s| SessionState::from_stored(&s.state).is_terminal())
    }

    /// A session the harness no longer has is not a session that is quietly
    /// idle. Move it to a terminal state with the reason, under the same guard
    /// every other writer uses: an ending already recorded stands.
    fn gone(&self, reason: &str) {
        let _ = self.store.opencode_set_gone(&self.session_id);
        let applied = self
            .store
            .update_session_unless(
                &self.session_id,
                SessionState::TERMINAL,
                SessionPatch {
                    state: Some(SessionState::Closed.as_str().to_string()),
                    end_reason: Some(EndReason::HarnessExit.as_str().to_string()),
                    last_error: Some(reason.to_string()),
                    ended_mono_ms: Some(self.mono_ms()),
                    turn_active: Some(false),
                    ..Default::default()
                },
            )
            .unwrap_or(false);
        if applied {
            self.record(
                ek::STATE,
                None,
                json!({
                    "state": SessionState::Closed.as_str(),
                    "end_reason": EndReason::HarnessExit.as_str(),
                    "reason": reason,
                }),
            );
        } else {
            self.record(ek::LATE_REFUSED, None, json!({ "what": "gone_upstream" }));
        }
        self.publish_session();
    }

    /// Everything the durable log holds past what has been ingested. Only on
    /// the startup path: with a stream running, `?after=` replays the same
    /// events through the adapter, and reading them here as well would be two
    /// readers racing for one sequence.
    async fn replay_history(
        &self,
        api: &dyn HarnessSnapshots,
        upstream: &str,
        report: &mut Report,
    ) {
        let mut finished: Option<Value> = None;
        for _ in 0..HISTORY_PAGES {
            let after = self.store.opencode_last_seq(&self.session_id).unwrap_or(0);
            let path =
                format!("/api/session/{upstream}/history?after={after}&limit={HISTORY_PAGE}");
            let Ok(page) = api.get(&path).await else {
                return;
            };
            let events: Vec<Value> = page["data"].as_array().cloned().unwrap_or_default();
            if events.is_empty() {
                break;
            }
            for event in &events {
                if self.ingest_snapshot_event(event) {
                    report.ingested += 1;
                }
                if event["type"].as_str() == Some("session.next.step.ended")
                    && event["data"]["finish"].as_str().unwrap_or_default() != "tool-calls"
                {
                    finished = Some(event["data"].clone());
                }
            }
            if page["hasMore"] != true {
                break;
            }
        }
        if let Some(step) = finished {
            report.turn_closed = self.close_turn(&step);
        }
    }

    /// A turn that ended while the node was away. The store still says a turn
    /// is active and no task is waiting for it, so it is closed here with the
    /// usage the harness recorded rather than left to a timeout.
    fn close_turn(&self, step: &Value) -> bool {
        let Ok(Some(session)) = self.store.get_session(&self.session_id) else {
            return false;
        };
        if session.turn_active == 0 {
            return false;
        }
        let tokens = usage_of(&step["tokens"]) as i64;
        let applied = self
            .store
            .update_session_unless(
                &self.session_id,
                SessionState::TERMINAL,
                SessionPatch {
                    turn_active: Some(false),
                    tokens_used: Some(session.tokens_used + tokens),
                    ..Default::default()
                },
            )
            .unwrap_or(false);
        if !applied {
            return false;
        }
        self.record(
            ek::TURN_END,
            None,
            json!({
                "stop_reason": step["finish"].as_str().unwrap_or("end_turn"),
                "usage": { "total_tokens": tokens },
                "source": "reconciled",
                "reason": "the turn ended while the node was disconnected",
            }),
        );
        self.publish_session();
        true
    }

    /// What a mutation did, asked rather than guessed. A prompt is never
    /// re-sent: a second one would duplicate a turn's work, so the question is
    /// whether the harness admitted the first. A permission reply *is*
    /// idempotent, so one still pending upstream is simply sent again.
    async fn settle_intents(
        &self,
        api: &dyn HarnessSnapshots,
        upstream: &str,
        report: &mut Report,
    ) {
        let Ok(intents) = self.store.opencode_unsettled_intents(&self.session_id) else {
            return;
        };
        if intents.is_empty() {
            self.clear_uncertain("nothing was in flight");
            return;
        }
        let messages = api
            .get(&format!("/api/session/{upstream}/message"))
            .await
            .unwrap_or(Value::Null);
        let pending = self.pending_permissions(api, upstream).await;
        let mut unresolved = 0usize;
        for intent in intents {
            match intent.kind.as_str() {
                intent_kind::PROMPT => {
                    let admitted = intent
                        .detail
                        .as_deref()
                        .is_some_and(|text| snapshot_mentions(&messages, text));
                    if admitted {
                        self.intent_settle(
                            &intent.id,
                            intent_state::ADMITTED,
                            Some("the harness holds this prompt; it was not sent again"),
                        );
                    } else {
                        self.intent_settle(
                            &intent.id,
                            intent_state::FAILED,
                            Some("the harness never admitted this prompt"),
                        );
                    }
                    report.settled += 1;
                }
                intent_kind::PERMISSION_REPLY => {
                    let target = intent.target.clone().unwrap_or_default();
                    if pending.iter().any(|p| p["id"].as_str() == Some(&target)) {
                        let body = reply_body(intent.detail.as_deref().unwrap_or_default());
                        let path = format!("/api/session/{upstream}/permission/{target}/reply");
                        match api.post(&path, body).await {
                            Ok(_) => {
                                self.intent_settle(
                                    &intent.id,
                                    intent_state::ADMITTED,
                                    Some("still pending upstream; the answer was sent again"),
                                );
                                report.resent += 1;
                                report.settled += 1;
                            }
                            Err(_) => unresolved += 1,
                        }
                    } else {
                        self.intent_settle(
                            &intent.id,
                            intent_state::ADMITTED,
                            Some("the harness is no longer waiting on this request"),
                        );
                        report.settled += 1;
                    }
                }
                // An abort is idempotent and its effect is observable in the
                // session's own state; nothing is re-sent on its account.
                _ => {
                    self.intent_settle(&intent.id, intent_state::ADMITTED, None);
                    report.settled += 1;
                }
            }
        }
        if unresolved == 0 {
            self.clear_uncertain("every mutation in flight was accounted for");
        }
    }

    async fn pending_permissions(&self, api: &dyn HarnessSnapshots, upstream: &str) -> Vec<Value> {
        api.get(&format!("/api/session/{upstream}/permission"))
            .await
            .ok()
            .and_then(|body| body["data"].as_array().cloned())
            .unwrap_or_default()
    }

    /// A permission the harness is blocked on, against what this node knows
    /// about it. Pending upstream and unanswered here goes back on the queue;
    /// pending upstream but already answered here is answered again, because
    /// the reply route is idempotent and the alternative is a harness blocked
    /// on a decision the operator already made.
    async fn reconcile_permissions(
        &self,
        api: &dyn HarnessSnapshots,
        upstream: &str,
        report: &mut Report,
    ) {
        let pending = self.pending_permissions(api, upstream).await;
        let known = self
            .store
            .permissions_for_session(&self.session_id)
            .unwrap_or_default();
        for request in pending {
            let Some(id) = request["id"].as_str() else {
                continue;
            };
            let call = request["source"]["callID"]
                .as_str()
                .or_else(|| request["tool"]["callID"].as_str());
            let answered = known.iter().find(|row| {
                row.state == "answered" && call.is_some() && row.tool_call_id.as_deref() == call
            });
            if let Some(row) = answered {
                let option = row.answer_option_id.clone().unwrap_or_default();
                let intent =
                    self.intent_begin(intent_kind::PERMISSION_REPLY, Some(id), Some(&option));
                let path = format!("/api/session/{upstream}/permission/{id}/reply");
                match api.post(&path, reply_body(&option)).await {
                    Ok(_) => {
                        self.intent_settle(
                            &intent,
                            intent_state::ADMITTED,
                            Some("re-sent an answer the harness had not taken"),
                        );
                        report.resent += 1;
                    }
                    Err(e) => self.intent_settle(
                        &intent,
                        intent_state::UNCERTAIN,
                        Some(&format!("re-sending the answer failed: {e}")),
                    ),
                }
                continue;
            }
            if known
                .iter()
                .any(|row| row.state != "answered" && row.tool_call_id.as_deref() == call)
            {
                continue; // already on the queue, waiting on the operator
            }
            let mapped = self
                .store
                .opencode_object(&self.session_id, object_kind::PERMISSION, id)
                .ok()
                .flatten();
            if mapped.as_ref().and_then(|m| m.state.clone()).as_deref() == Some("raised") {
                continue;
            }
            if self.reraise(upstream, id, &request).await {
                report.reraised += 1;
            }
        }
    }

    /// Put one upstream request back on the node's queue. It goes in through
    /// the supervisor's own command channel, so the policy, the card, the
    /// expiry and the answer path are the ones every other request gets; what
    /// is different is only that the answer is posted from here, because the
    /// adapter that raised it originally is no longer waiting for it.
    async fn reraise(&self, upstream: &str, id: &str, request: &Value) -> bool {
        let action = request["action"].as_str().unwrap_or("a tool").to_string();
        let resources: Vec<String> = request["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(str::to_string))
            .collect();
        let (reply_tx, reply_rx) = oneshot::channel();
        let ask = Command::Permission {
            request: PermissionRequest {
                tool_call_id: request["source"]["callID"].as_str().map(str::to_string),
                title: format!("{action}: {}", resources.join(", ")),
                kind: Some("tool".into()),
                raw_input: Some(json!({
                    "action": action,
                    "resources": resources,
                    "metadata": request["metadata"].clone(),
                    "reraised": true,
                })),
                options: allow_or_reject(),
            },
            reply: reply_tx,
        };
        if self.commands.send(ask).await.is_err() {
            return false;
        }
        let _ =
            self.store
                .opencode_map(&self.session_id, object_kind::PERMISSION, id, None, None, 0);
        let _ = self.store.opencode_set_object_state(
            &self.session_id,
            object_kind::PERMISSION,
            id,
            "raised",
        );
        self.record(
            ek::PERMISSION_REQUEST,
            Some(id.to_string()),
            json!({
                "upstream_id": id,
                "title": action,
                "reraised": true,
                "reason": "the harness is still waiting on this request and the node was not",
            }),
        );

        // The answer is a mutation like any other: written down before it is
        // sent, and re-sent rather than asked about if it does not report.
        let api = self.api();
        let store = self.store.clone();
        let session_id = self.session_id.clone();
        let upstream = upstream.to_string();
        let permission = id.to_string();
        tokio::spawn(async move {
            let decision = reply_rx.await.unwrap_or(PermissionReply::Cancelled);
            let option = match decision {
                PermissionReply::Selected(option) => option,
                PermissionReply::Edited { option_id, .. } => option_id,
                PermissionReply::Cancelled => crate::acp::types::OPTION_REJECT_ONCE.to_string(),
            };
            let intent = uuid::Uuid::now_v7().to_string();
            let _ = store.opencode_intent_begin(
                &intent,
                &session_id,
                intent_kind::PERMISSION_REPLY,
                Some(&permission),
                Some(&option),
            );
            let _ = store.opencode_set_object_state(
                &session_id,
                object_kind::PERMISSION,
                &permission,
                "answered",
            );
            let Some(api) = api else { return };
            let path = format!("/api/session/{upstream}/permission/{permission}/reply");
            match api.post(&path, reply_body(&option)).await {
                Ok(_) => {
                    let _ = store.opencode_intent_settle(&intent, intent_state::ADMITTED, None);
                }
                Err(e) => {
                    // Idempotent: the next reconciliation sends it again
                    // rather than leaving the harness blocked.
                    let _ = store.opencode_intent_settle(
                        &intent,
                        intent_state::UNCERTAIN,
                        Some(&format!("answering the harness failed: {e}")),
                    );
                }
            }
        });
        true
    }
}

/// The two options every harness permission is offered with. Allow-once and
/// reject-once and nothing else: an `always` is a grant the node would have to
/// mean, and it never does (finding 2).
fn allow_or_reject() -> Vec<crate::acp::types::PermissionOption> {
    use crate::acp::types::{PermissionOption, OPTION_ALLOW_ONCE, OPTION_REJECT_ONCE};
    vec![
        PermissionOption {
            option_id: OPTION_ALLOW_ONCE.into(),
            name: "Allow".into(),
            kind: "allow_once".into(),
        },
        PermissionOption {
            option_id: OPTION_REJECT_ONCE.into(),
            name: "Reject".into(),
            kind: "reject_once".into(),
        },
    ]
}

#[async_trait]
impl DurableCursor for Ingest {
    async fn bind(&self, upstream_session_id: &str, api: Arc<dyn HarnessSnapshots>) {
        self.rebind(upstream_session_id, api);
    }

    async fn resume_from(&self) -> u64 {
        self.store
            .opencode_last_seq(&self.session_id)
            .unwrap_or(0)
            .max(0) as u64
    }

    async fn admit(&self, event: &Value) -> bool {
        let Some(seq) = event["durable"]["seq"].as_u64() else {
            // Not a durable event: the server-wide stream carries no sequence
            // at all (finding 6) and is a live tap, so it is passed through
            // and made idempotent by what it produces, not by a number.
            return true;
        };
        self.map_identities(event, seq as i64);
        self.admit_seq(seq)
    }

    async fn reconnected(&self) {
        self.reconcile(Reconcile::Reconnect).await;
    }
}

/// Which OpenCode namespace an id belongs to. Unknown prefixes are not
/// guessed at: an id the node cannot name is an id it does not map.
fn object_of(id: &str) -> Option<&'static str> {
    Some(match id.split('_').next().unwrap_or_default() {
        "msg" => object_kind::MESSAGE,
        "prt" => object_kind::PART,
        "per" => object_kind::PERMISSION,
        "que" => object_kind::QUESTION,
        "pty" => object_kind::PTY,
        _ => return None,
    })
}

/// What `reply` the harness is sent. Allow-once is the only option that
/// allows, and `always` is never sent: it would widen OpenCode's own ruleset
/// behind the node's back (finding 2).
fn reply_body(option_id: &str) -> Value {
    if option_id == crate::acp::types::OPTION_ALLOW_ONCE {
        json!({ "reply": "once" })
    } else {
        json!({ "reply": "reject" })
    }
}

/// Whether a snapshot holds a prompt's own text. Compared against the JSON
/// encoding of the text rather than the raw string, so a prompt with a
/// newline or a quote in it still matches what the harness stored.
fn snapshot_mentions(snapshot: &Value, text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let needle = serde_json::to_string(text).unwrap_or_default();
    let needle = needle.trim_matches('"');
    !needle.is_empty() && snapshot.to_string().contains(needle)
}

/// The tokens one step charged, summed the way the adapter sums them: input,
/// output, reasoning and both cache figures are disjoint in OpenCode's report.
fn usage_of(tokens: &Value) -> u64 {
    let field = |name: &str| tokens[name].as_u64().unwrap_or(0);
    field("input")
        + field("output")
        + field("reasoning")
        + tokens["cache"]["read"].as_u64().unwrap_or(0)
        + tokens["cache"]["write"].as_u64().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_mapped_only_when_its_namespace_is_known() {
        assert_eq!(object_of("msg_abc"), Some(object_kind::MESSAGE));
        assert_eq!(object_of("prt_abc"), Some(object_kind::PART));
        assert_eq!(object_of("per_abc"), Some(object_kind::PERMISSION));
        assert_eq!(object_of("que_abc"), Some(object_kind::QUESTION));
        assert_eq!(object_of("pty_abc"), Some(object_kind::PTY));
        // A session id is not an object, and a provider's call id is not an
        // identity at all.
        assert_eq!(object_of("ses_abc"), None);
        assert_eq!(object_of("call_1"), None);
        assert_eq!(object_of("toolu_01"), None);
    }

    #[test]
    fn only_allow_once_allows_and_always_is_never_sent() {
        assert_eq!(
            reply_body(crate::acp::types::OPTION_ALLOW_ONCE)["reply"],
            "once"
        );
        assert_eq!(reply_body("allow_always")["reply"], "reject");
        assert_eq!(
            reply_body(crate::acp::types::OPTION_REJECT_ONCE)["reply"],
            "reject"
        );
    }

    #[test]
    fn a_prompt_is_recognised_in_a_snapshot_through_its_own_escaping() {
        let snapshot = json!({ "data": [
            { "info": { "role": "user" }, "parts": [{ "type": "text", "text": "fix the \"bug\"\nnow" }] }
        ]});
        assert!(snapshot_mentions(&snapshot, "fix the \"bug\"\nnow"));
        assert!(!snapshot_mentions(&snapshot, "write the docs"));
        assert!(!snapshot_mentions(&snapshot, ""));
    }

    #[test]
    fn a_step_charges_every_disjoint_field() {
        let tokens = json!({
            "input": 1000, "output": 20, "reasoning": 4,
            "cache": { "read": 8, "write": 2 },
        });
        assert_eq!(usage_of(&tokens), 1034);
    }
}
