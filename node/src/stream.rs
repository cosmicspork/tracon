//! The event stream the interface consumes. Persisted events carry their store
//! `seq` as the SSE id, so a client that reconnects with `Last-Event-ID` gets
//! exactly what it missed. Ephemeral frames (live chunks, tool progress,
//! snapshots) carry no id: they are superseded by the persisted event or by a
//! refetch, and replaying them would be noise.

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::store::{PermissionRow, SessionRow};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Event {
        seq: i64,
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
#[derive(Clone)]
pub struct Hub {
    tx: broadcast::Sender<Frame>,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { tx }
    }

    pub fn publish(&self, frame: Frame) {
        // An error here only means nobody is listening.
        let _ = self.tx.send(frame);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Frame> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(seq: i64) -> Frame {
        Frame::Event {
            seq,
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
        let hub = Hub::new();
        let mut rx = hub.subscribe();
        hub.publish(event(1));
        let got = rx.recv().await.unwrap();
        assert_eq!(got.id(), Some(1));
    }

    #[tokio::test]
    async fn publishing_with_no_subscribers_is_not_an_error() {
        Hub::new().publish(event(1));
    }
}
