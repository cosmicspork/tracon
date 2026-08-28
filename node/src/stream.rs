//! The event stream the interface consumes. Persisted events carry their store
//! `seq` as the SSE id, so a client that reconnects with `Last-Event-ID` gets
//! exactly what it missed. Ephemeral frames (live chunks, tool progress,
//! snapshots) carry no id: they are superseded by the persisted event or by a
//! refetch, and replaying them would be noise.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::store::{PermissionRow, SessionRow};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Event {
        seq: i64,
        /// The node that owns the session. Local events carry this node's id;
        /// mirrored ones carry the owner's.
        node_id: String,
        session_id: String,
        kind: String,
        ref_id: Option<String>,
        payload: Value,
        at_ms: i64,
    },
    Chunk {
        session_id: String,
        message_id: Option<String>,
        kind: &'static str,
        text: String,
    },
    ToolUpdate {
        session_id: String,
        tool_call_id: String,
        status: Option<String>,
    },
    Session(Box<SessionRow>),
    Queue {
        waiting: Vec<PermissionRow>,
    },
    Reviews {
        waiting: Vec<crate::store::ReviewRow>,
    },
    Node(Value),
    /// Hub reachability and mesh counters, for the banner.
    Mesh(Value),
    /// Every configured model provider and its state on this node.
    Providers {
        providers: Vec<Value>,
    },
    /// Record changes: local ones on their way to the mesh, or mirrored ones
    /// that won here, for the interface.
    Changes {
        channel: String,
        changes: Vec<proto::frame::Change>,
    },
}

impl Frame {
    /// The SSE event name.
    pub fn name(&self) -> &'static str {
        match self {
            Frame::Event { .. } => "event",
            Frame::Chunk { .. } => "chunk",
            Frame::ToolUpdate { .. } => "tool_update",
            Frame::Session(_) => "session",
            Frame::Queue { .. } => "queue",
            Frame::Reviews { .. } => "reviews",
            Frame::Node(_) => "node",
            Frame::Mesh(_) => "mesh",
            Frame::Providers { .. } => "providers",
            Frame::Changes { .. } => "changes",
        }
    }

    /// Only persisted events are replayable, so only they carry an id.
    pub fn id(&self) -> Option<i64> {
        match self {
            Frame::Event { seq, .. } => Some(*seq),
            _ => None,
        }
    }
}

/// Fan-out to connected clients. Slow clients lag and are told to resync rather
/// than holding the node's memory.
///
/// A tap, when set, receives every frame this node originates so the mesh
/// client can forward it. Mirrored frames (state that arrived from a peer) are
/// published untapped, which is what keeps a frame from looping back out.
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Frame>,
    tap: Arc<Mutex<Option<mpsc::Sender<Frame>>>>,
    /// Fires when the node is shutting down. SSE streams end when it does, so a
    /// browser holding one open does not stall graceful shutdown (a keep-alive
    /// stream never completes on its own).
    shutdown: CancellationToken,
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            tx,
            tap: Arc::new(Mutex::new(None)),
            shutdown: CancellationToken::new(),
        }
    }

    /// Route every locally originated frame to `tap` as well as to subscribers.
    pub fn with_tap(&self, tap: mpsc::Sender<Frame>) {
        *self.tap.lock().unwrap() = Some(tap);
    }

    /// Publish a frame this node originated: subscribers and the tap.
    pub fn publish(&self, frame: Frame) {
        if let Some(tap) = self.tap.lock().unwrap().as_ref() {
            // The tap consumer persists to an outbox promptly; a full buffer
            // means it has stalled, and dropping here beats blocking a session.
            if let Err(e) = tap.try_send(frame.clone()) {
                tracing::warn!(frame = frame.name(), error = %e, "mesh tap dropped a frame");
            }
        }
        // An error here only means nobody is listening.
        let _ = self.tx.send(frame);
    }

    /// Publish a frame that arrived from a peer: subscribers only, never the
    /// tap, so mirrored state does not echo back onto the mesh.
    pub fn publish_untapped(&self, frame: Frame) {
        let _ = self.tx.send(frame);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Frame> {
        self.tx.subscribe()
    }

    /// A handle that resolves when the node begins shutting down.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Signal every open stream to end.
    pub fn begin_shutdown(&self) {
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(seq: i64) -> Frame {
        Frame::Event {
            seq,
            node_id: "n".into(),
            session_id: "s".into(),
            kind: "message".into(),
            ref_id: None,
            payload: json!({}),
            at_ms: 0,
        }
    }

    #[test]
    fn only_persisted_events_are_replayable() {
        assert_eq!(event(7).id(), Some(7));
        assert_eq!(event(7).name(), "event");
        let chunk = Frame::Chunk {
            session_id: "s".into(),
            message_id: None,
            kind: "message",
            text: "hi".into(),
        };
        assert_eq!(chunk.id(), None);
        assert_eq!(chunk.name(), "chunk");
    }

    #[tokio::test]
    async fn subscribers_receive_published_frames() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        bus.publish(event(1));
        let got = rx.recv().await.unwrap();
        assert_eq!(got.id(), Some(1));
    }

    #[tokio::test]
    async fn publishing_with_no_subscribers_is_not_an_error() {
        Bus::new().publish(event(1));
    }

    #[tokio::test]
    async fn tap_sees_published_but_not_untapped_frames() {
        let bus = Bus::new();
        let (tx, mut rx) = mpsc::channel(4);
        bus.with_tap(tx);
        let mut sub = bus.subscribe();
        bus.publish(event(1));
        bus.publish_untapped(event(2));
        assert_eq!(rx.recv().await.unwrap().id(), Some(1));
        assert!(rx.try_recv().is_err());
        assert_eq!(sub.recv().await.unwrap().id(), Some(1));
        assert_eq!(sub.recv().await.unwrap().id(), Some(2));
    }
}
