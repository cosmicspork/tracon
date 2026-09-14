//! The live channel the native UI actually opens, synthesised by the node.
//!
//! The native app has exactly one streaming connection: `GET /global/event`
//! (`docs/reference/opencode-v1.18.30/README.md` finding 20, captured in
//! `ui-route-trace.tsv`). It opens it at startup, reopens it whenever it
//! drops, and opens nothing else that streams — it never asks for the
//! per-session durable stream. So every live thing the page shows, a
//! permission request above all, arrives on that one socket or not at all.
//!
//! Upstream's own `/global/event` is not a stream tracon can forward.
//! It is **unscoped** — a firehose across every instance and session on the
//! server, and one OpenCode server holds more than one session — and it has
//! **no durable replay**: `handlers/event.ts` builds every frame with
//! `id: undefined`, so `Last-Event-ID` cannot work and anything published
//! while a client is disconnected is gone (finding 6). Forwarding it would
//! hand one session's browser another session's transcript and still not be
//! reconcilable. It stays refused.
//!
//! So the node serves that route instead of forwarding it. This module is the
//! node's side of it: one bounded, sequenced, session-scoped log per session,
//! written by the streams the node **already** reads and read by
//! `gateway::opencode`.
//!
//! Three things make it safe to serve where the upstream one was not.
//!
//! **One upstream reader.** Nothing here opens a connection. The adapter's two
//! pumps (`adapter::opencode`) are the only readers of the harness's streams,
//! and they already exist: the durable per-session stream that ingestion is
//! anchored on, and the server-wide `/api/event` that carries the asks the
//! durable one does not (finding 6). Both hand every event they see to
//! `DurableCursor::observe`, which lands here. A second raw reader for one
//! sequence is exactly what the sequence exists to prevent, so there is not
//! one.
//!
//! **Scope is checked, not assumed.** A frame is published only if it *names*
//! this session — in `data.sessionID`, in the part or info it carries, or as
//! the durable aggregate. An event that names no session is dropped rather
//! than broadcast, so a global, config, auth or another session's event
//! cannot reach a browser even if a future release starts publishing it on a
//! stream the node reads. The allowlist of types is closed for the same
//! reason.
//!
//! **It replays.** Frames are numbered by the node, kept in a ring, and served
//! with that number as the SSE `id:`. A client that reconnects with
//! `Last-Event-ID` gets what it missed; one that has never connected starts at
//! the head. The numbering is tracon's, not upstream's, because upstream has
//! none — which is the half of finding 6 that made the route unforwardable and
//! is fixed here rather than wished away. Correctness still rests on the
//! durable stream and ingestion; this is the UI's view of it, and a frame the
//! ring has dropped is a frame the page refetches.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};
use tokio::sync::broadcast;

/// How many frames one session keeps for a client that reconnects. A browser
/// that was away longer than this refetches from the readable snapshot routes,
/// which is what it does on a cold load anyway.
const RING: usize = 512;

/// The event types the synthesised stream carries, as the v1 client that the
/// native app is spells them (`README.md` finding 20). Closed on purpose:
/// anything not named here is dropped, so a release that adds an event about
/// the server, the installation, the catalogue, another project or a PTY does
/// not start reaching a browser because nobody remembered to deny it.
///
/// `server.connected` and `server.heartbeat` are absent because they are not
/// forwarded — the node mints its own, per connection, in `gateway::opencode`.
const READABLE: &[&str] = &[
    // The session itself. Every one of these carries its own `sessionID` and
    // is dropped unless it is this one's.
    "session.updated",
    "session.deleted",
    "session.idle",
    "session.error",
    "session.diff",
    "session.status",
    "session.compacted",
    // Its transcript.
    "message.updated",
    "message.removed",
    "message.part.updated",
    "message.part.delta",
    "message.part.removed",
    // The asks. This is the point of the exercise: without these the page
    // never shows a permission control (finding 20).
    "permission.asked",
    "permission.replied",
    "question.asked",
    "question.replied",
    "question.rejected",
    // Its todo list and the files it edited.
    "todo.updated",
    "file.edited",
];

/// One frame on the synthesised stream: tracon's sequence number and the
/// envelope the app parses.
#[derive(Debug, Clone)]
pub struct Frame {
    /// The SSE `id:`. Monotonic per session, and tracon's own — upstream emits
    /// none at all (finding 6).
    pub id: u64,
    /// `{ id, type, properties }`, the shape the app's legacy path reads out
    /// of the global envelope's `payload`
    /// (`packages/app/src/context/server-sdk.tsx`).
    pub payload: Value,
}

/// One session's synthesised live channel.
pub struct NativeEvents {
    tx: broadcast::Sender<Arc<Frame>>,
    ring: Mutex<VecDeque<Arc<Frame>>>,
    next: AtomicU64,
}

impl NativeEvents {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(RING);
        Arc::new(Self {
            tx,
            ring: Mutex::new(VecDeque::with_capacity(RING)),
            next: AtomicU64::new(0),
        })
    }

    /// Offer one event read off a harness stream. `upstream_session` is the
    /// OpenCode session this channel belongs to; an event that does not name
    /// it is not published.
    ///
    /// Returns whether it was published, which is what the tests assert
    /// against rather than reading the ring.
    pub fn offer(&self, upstream_session: &str, event: &Value) -> bool {
        let Some(payload) = envelope(event) else {
            return false;
        };
        if !names_session(event, upstream_session) {
            return false;
        }
        self.publish(payload)
    }

    /// Put one already-shaped envelope on the stream. The connection frames
    /// the gateway mints for a single client do **not** come through here —
    /// they are per-connection, not per-session.
    fn publish(&self, payload: Value) -> bool {
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let frame = Arc::new(Frame { id, payload });
        {
            let mut ring = self.ring.lock().unwrap();
            if ring.len() == RING {
                ring.pop_front();
            }
            ring.push_back(frame.clone());
        }
        // No receiver is the ordinary case: nobody has the page open. The
        // ring is what a later connection reads, so this is not a failure.
        let _ = self.tx.send(frame);
        true
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Frame>> {
        self.tx.subscribe()
    }

    /// The highest id published so far. A client that sends no resume point
    /// starts here: the durable stream still guarantees ingestion, so a live
    /// view that begins at the head loses nothing the node needed.
    pub fn head(&self) -> u64 {
        self.next.load(Ordering::SeqCst)
    }

    /// Everything still held after `id`, oldest first, for a client that
    /// reconnected with a `Last-Event-ID`. A point older than the ring yields
    /// what is left rather than an error: the page refetches the rest from the
    /// snapshot routes, exactly as it does on a cold load.
    pub fn replay(&self, after: u64) -> Vec<Arc<Frame>> {
        self.ring
            .lock()
            .unwrap()
            .iter()
            .filter(|f| f.id > after)
            .cloned()
            .collect()
    }
}

/// The `{ id, type, properties }` envelope for one upstream event, or `None`
/// if this is not an event the stream carries.
///
/// Two upstream spellings reach the node's pumps for the same fact: the v1
/// names on the server-wide stream and the `*.v2.*` names beside them. The app
/// reading the *global* envelope does **not** run its own v2 adapter — that
/// runs only on the v2 transport — so the normalisation the app would have
/// done (`adaptServerEvent`) is done here, and the page sees one vocabulary.
fn envelope(event: &Value) -> Option<Value> {
    let kind = event["type"].as_str()?;
    // The durable stream spells the body `data`; a `/global/event` frame that
    // was already in the legacy shape spells it `properties`.
    let data = match &event["data"] {
        Value::Null => &event["properties"],
        data => data,
    };
    let id = event["id"].as_str().unwrap_or_default();

    let (kind, properties) = match kind {
        "permission.v2.asked" => ("permission.asked", v1_permission(data)),
        "permission.v2.replied" => ("permission.replied", data.clone()),
        "question.v2.asked" => ("question.asked", data.clone()),
        "question.v2.replied" => ("question.replied", data.clone()),
        "question.v2.rejected" => ("question.rejected", data.clone()),
        other => (other, data.clone()),
    };
    if !READABLE.contains(&kind) {
        return None;
    }
    Some(json!({
        "id": if id.is_empty() { Value::Null } else { json!(id) },
        "type": kind,
        "properties": properties,
    }))
}

/// A v2 permission request in the v1 shape the app destructures, field for
/// field as `adaptServerEvent` does it: `action` becomes `permission`,
/// `resources` become `patterns`, `save` becomes `always`, and a tool source
/// becomes `{messageID, callID}`.
fn v1_permission(data: &Value) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), data["id"].clone());
    out.insert("sessionID".into(), data["sessionID"].clone());
    out.insert("permission".into(), data["action"].clone());
    out.insert("patterns".into(), data["resources"].clone());
    out.insert(
        "always".into(),
        match &data["save"] {
            Value::Null => json!([]),
            save => save.clone(),
        },
    );
    out.insert(
        "metadata".into(),
        match &data["metadata"] {
            Value::Null => json!({}),
            metadata => metadata.clone(),
        },
    );
    if data["source"]["type"] == "tool" {
        out.insert(
            "tool".into(),
            json!({
                "messageID": data["source"]["messageID"].clone(),
                "callID": data["source"]["callID"].clone(),
            }),
        );
    }
    Value::Object(out)
}

/// Whether this event names the session the channel belongs to.
///
/// Fail closed: an event that names no session at all is not published. The
/// places a session id is written are few and each is an upstream schema fact
/// (`api-ui.md` §3, "Events carrying these ids"), so a shape that stops
/// naming one is a shape the node stops forwarding rather than one it guesses
/// about.
fn names_session(event: &Value, session: &str) -> bool {
    let data = match &event["data"] {
        Value::Null => &event["properties"],
        data => data,
    };
    [
        event["durable"]["aggregateID"].as_str(),
        data["sessionID"].as_str(),
        data["info"]["sessionID"].as_str(),
        data["info"]["id"].as_str(),
        data["part"]["sessionID"].as_str(),
        data["item"]["sessionID"].as_str(),
    ]
    .into_iter()
    .flatten()
    .any(|named| named == session)
}

/// Whether one pending-ask record belongs to this session.
///
/// The snapshot routes the app loads a page from — `GET /permission`,
/// `GET /question` and their v2 spellings — are **instance**-wide, not
/// session-scoped: one OpenCode server holds more than one session, and the
/// path carries no session at all. So the gateway filters their bodies with
/// this, and a permission raised in another session is not on the page.
pub fn ask_is_mine(record: &Value, session: &str) -> bool {
    record["sessionID"].as_str() == Some(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINE: &str = "ses_mine";

    fn v1_asked(session: &str) -> Value {
        json!({
            "id": "evt_1",
            "type": "permission.asked",
            "properties": {
                "id": "per_1",
                "sessionID": session,
                "permission": "bash",
                "patterns": ["just test"],
                "metadata": { "command": "just test" },
                "always": [],
                "tool": { "messageID": "msg_1", "callID": "call_1" },
            },
        })
    }

    #[test]
    fn an_ask_for_this_session_is_published_and_another_session_is_not() {
        let events = NativeEvents::new();
        assert!(events.offer(MINE, &v1_asked(MINE)));
        assert!(!events.offer(MINE, &v1_asked("ses_other")));
        assert_eq!(events.head(), 1);

        let held = events.replay(0);
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].payload["type"], "permission.asked");
        assert_eq!(held[0].payload["properties"]["sessionID"], MINE);
    }

    #[test]
    fn a_v2_ask_reaches_the_page_in_the_v1_shape_the_app_reads() {
        let events = NativeEvents::new();
        assert!(events.offer(
            MINE,
            &json!({
                "id": "evt_9",
                "type": "permission.v2.asked",
                "data": {
                    "id": "per_2",
                    "sessionID": MINE,
                    "action": "bash",
                    "resources": ["just test"],
                    "metadata": { "command": "just test" },
                    "source": { "type": "tool", "messageID": "msg_1", "callID": "call_1" },
                },
            })
        ));
        let frame = &events.replay(0)[0].payload;
        // The name the app's legacy path switches on, not the v2 one: the
        // global envelope is not run through the app's own v2 adapter.
        assert_eq!(frame["type"], "permission.asked");
        assert_eq!(frame["properties"]["permission"], "bash");
        assert_eq!(frame["properties"]["patterns"], json!(["just test"]));
        assert_eq!(frame["properties"]["always"], json!([]));
        assert_eq!(frame["properties"]["tool"]["callID"], "call_1");
    }

    #[test]
    fn an_event_that_names_no_session_is_not_broadcast() {
        let events = NativeEvents::new();
        // Shaped like something the page would happily render, but nothing
        // says whose it is.
        assert!(!events.offer(
            MINE,
            &json!({ "type": "session.updated", "properties": { "info": {} } })
        ));
        assert_eq!(events.head(), 0);
    }

    #[test]
    fn global_config_and_capability_events_are_not_on_the_stream() {
        let events = NativeEvents::new();
        for kind in [
            "server.instance.disposed",
            "global.disposed",
            "installation.update-available",
            "catalog.updated",
            "integration.connection.updated",
            "plugin.added",
            "mcp.tools.changed",
            "lsp.updated",
            "pty.created",
            "project.updated",
            "file.watcher.updated",
            "tui.command.execute",
            // The node mints these itself, per connection; it never relays
            // another connection's.
            "server.connected",
            "server.heartbeat",
        ] {
            assert!(
                !events.offer(MINE, &json!({ "type": kind, "data": { "sessionID": MINE } })),
                "{kind} must not reach a browser"
            );
        }
        assert_eq!(events.head(), 0);
    }

    #[test]
    fn the_durable_aggregate_names_the_session_too() {
        let events = NativeEvents::new();
        assert!(events.offer(
            MINE,
            &json!({
                "id": "evt_2",
                "type": "todo.updated",
                "durable": { "aggregateID": MINE, "seq": 4, "version": 1 },
                "data": { "todos": [] },
            })
        ));
        assert!(!events.offer(
            MINE,
            &json!({
                "type": "todo.updated",
                "durable": { "aggregateID": "ses_other", "seq": 4, "version": 1 },
                "data": { "todos": [] },
            })
        ));
        assert_eq!(events.head(), 1);
    }

    #[test]
    fn ids_are_the_nodes_own_and_replay_resumes_after_one() {
        let events = NativeEvents::new();
        for _ in 0..3 {
            assert!(events.offer(MINE, &v1_asked(MINE)));
        }
        assert_eq!(events.head(), 3);
        let ids: Vec<u64> = events.replay(0).iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        let ids: Vec<u64> = events.replay(2).iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![3]);
        assert!(events.replay(3).is_empty());
    }

    #[test]
    fn the_ring_is_bounded_and_the_oldest_frames_fall_off() {
        let events = NativeEvents::new();
        for _ in 0..(RING + 10) {
            assert!(events.offer(MINE, &v1_asked(MINE)));
        }
        let held = events.replay(0);
        assert_eq!(held.len(), RING);
        // What is left is the newest, and its ids are still the node's own.
        assert_eq!(held[0].id, 11);
        assert_eq!(held[held.len() - 1].id, (RING + 10) as u64);
    }

    #[test]
    fn a_pending_ask_is_filtered_to_this_session() {
        assert!(ask_is_mine(&json!({ "sessionID": MINE }), MINE));
        assert!(!ask_is_mine(&json!({ "sessionID": "ses_other" }), MINE));
        // No session named is not this session's.
        assert!(!ask_is_mine(&json!({ "id": "per_1" }), MINE));
    }
}
