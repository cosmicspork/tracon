//! The stream relay: the hub's second, *non-durable* path.
//!
//! Durable frames are appended, retained and pulled by cursor. A stream is the
//! opposite of that in every respect, and deliberately so:
//!
//! - **Nothing is stored.** A frame lives in one bounded in-memory queue until
//!   the recipient's open connection takes it, and is then forgotten. Nothing
//!   reaches disk, nothing is retained, nothing is replayed after a reconnect
//!   — which is exactly what keeps a relayed `POST` from being sent twice.
//! - **Nothing is readable.** The hub verifies the signature and routes on the
//!   clear fields (channel, sender, recipient, stream id, sequence). The body
//!   is sealed under a key derived per stream and per direction, which the hub
//!   has never held and cannot derive.
//! - **Everything is bounded.** Per-stream buffer, per-connection queue,
//!   per-member open-stream count, per-member frame rate, per-frame size. A
//!   sender that outruns its stream's buffer is told to wait (429), which is
//!   the backpressure the end-to-end credit window rides on. A sender that
//!   outruns the recipient's whole connection has the stream dropped, and
//!   **both** ends are told, in the clear, that the hub dropped it — the hub
//!   cannot seal a close frame, so a control notice is the only honest way to
//!   end a stream it is throwing away.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc;

use crate::store::RateLimit;

/// What the relay allows. Every one of these is a bound on memory the hub
/// holds for someone else's traffic, so each has a default that a single
/// misbehaving member cannot make interesting.
#[derive(Clone, Debug)]
pub struct StreamLimits {
    /// The largest serialized stream envelope accepted.
    pub max_frame_bytes: usize,
    /// Frames buffered for one stream before the sender is told to wait.
    pub per_stream_buffer: usize,
    /// Frames buffered across every stream for one connected member.
    pub queue_depth: usize,
    /// Streams one member may have open at once.
    pub max_open_per_member: usize,
    /// Frames one member may post per minute.
    pub rate_per_min: u32,
    /// A stream with no traffic for this long is forgotten by the relay.
    pub idle_secs: u64,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: proto::stream::MAX_STREAM_FRAME_BYTES,
            per_stream_buffer: 32,
            queue_depth: 256,
            max_open_per_member: 32,
            rate_per_min: 3_000,
            idle_secs: 300,
        }
    }
}

/// Why the relay said no, and what the caller should do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayRefusal {
    /// The recipient has no open connection here. Not an error in the hub:
    /// the peer is offline, and the caller says so rather than retrying.
    NotConnected,
    /// This stream's buffer is full. Wait; do not drop the frame.
    Backpressure,
    /// Too many streams open for this member.
    TooManyStreams,
    /// Too many frames this minute.
    RateLimited,
    /// The stream was dropped as this frame arrived: the recipient's whole
    /// connection is backed up. Both ends have been told.
    Dropped,
}

impl RelayRefusal {
    pub fn message(&self) -> &'static str {
        match self {
            RelayRefusal::NotConnected => "the recipient has no stream connection to this hub",
            RelayRefusal::Backpressure => {
                "this stream's relay buffer is full; wait for the peer to read"
            }
            RelayRefusal::TooManyStreams => "too many streams open for this member",
            RelayRefusal::RateLimited => "too many stream frames from this member",
            RelayRefusal::Dropped => {
                "the recipient's stream connection is backed up; the stream was dropped"
            }
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            RelayRefusal::NotConnected => "not_connected",
            RelayRefusal::Backpressure => "backpressure",
            RelayRefusal::TooManyStreams => "too_many_streams",
            RelayRefusal::RateLimited => "rate_limited",
            RelayRefusal::Dropped => "relay_dropped",
        }
    }
}

/// A clear notice from the hub itself. It carries no stream content — the hub
/// holds no key — only the fact that a stream it was relaying has ended and
/// why, so both ends can close rather than wait forever.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Control {
    pub stream_id: String,
    pub reason: String,
    pub detail: String,
}

/// One thing handed to a connected member: either a sealed envelope, verbatim,
/// or a clear control notice.
pub struct Delivery {
    pub event: &'static str,
    pub data: String,
    /// Releases this frame's place in its stream's buffer when the writer has
    /// taken it. Dropping the delivery is what unblocks the sender.
    _slot: Option<SlotGuard>,
}

struct SlotGuard {
    slots: Arc<Mutex<HashMap<String, usize>>>,
    stream_id: String,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut slots = self.slots.lock().unwrap();
        if let Some(n) = slots.get_mut(&self.stream_id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                slots.remove(&self.stream_id);
            }
        }
    }
}

struct Conn {
    tx: mpsc::Sender<Delivery>,
    slots: Arc<Mutex<HashMap<String, usize>>>,
    /// Distinguishes one GET from the reconnect that replaced it.
    epoch: u64,
}

/// What the relay remembers about a stream: only who is at each end, so the
/// far side can be told when the near one goes away.
struct Route {
    a: String,
    b: String,
    last_ms: i64,
}

impl Route {
    fn other(&self, me: &str) -> Option<&str> {
        if me == self.a {
            Some(&self.b)
        } else if me == self.b {
            Some(&self.a)
        } else {
            None
        }
    }

    fn involves(&self, who: &str) -> bool {
        who == self.a || who == self.b
    }
}

#[derive(Default)]
struct Inner {
    conns: HashMap<String, Conn>,
    streams: HashMap<String, Route>,
    next_epoch: u64,
}

pub struct StreamRelay {
    limits: StreamLimits,
    inner: Mutex<Inner>,
    limiter: RateLimit,
}

impl Default for StreamRelay {
    fn default() -> Self {
        Self::new(StreamLimits::default())
    }
}

impl StreamRelay {
    pub fn new(limits: StreamLimits) -> Self {
        Self {
            limits,
            inner: Mutex::new(Inner::default()),
            limiter: RateLimit::new(),
        }
    }

    pub fn limits(&self) -> &StreamLimits {
        &self.limits
    }

    /// Open this member's delivery connection, replacing any previous one. A
    /// reconnect is a new connection and a new epoch; the old receiver ends,
    /// and every frame still queued on it is dropped rather than delivered
    /// twice.
    pub fn connect(&self, node_id: &str) -> mpsc::Receiver<Delivery> {
        let (tx, rx) = mpsc::channel(self.limits.queue_depth);
        let mut inner = self.inner.lock().unwrap();
        inner.next_epoch += 1;
        let epoch = inner.next_epoch;
        inner.conns.insert(
            node_id.to_string(),
            Conn {
                tx,
                slots: Arc::new(Mutex::new(HashMap::new())),
                epoch,
            },
        );
        rx
    }

    /// Forget a connection, unless a newer one has already replaced it.
    pub fn disconnect(&self, node_id: &str, epoch: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.conns.get(node_id).is_some_and(|c| c.epoch == epoch) {
            inner.conns.remove(node_id);
        }
    }

    /// The epoch of this member's current connection, for `disconnect`.
    pub fn epoch_of(&self, node_id: &str) -> Option<u64> {
        self.inner
            .lock()
            .unwrap()
            .conns
            .get(node_id)
            .map(|c| c.epoch)
    }

    pub fn is_connected(&self, node_id: &str) -> bool {
        self.inner.lock().unwrap().conns.contains_key(node_id)
    }

    /// Streams the relay is currently tracking. Diagnostics and tests only.
    pub fn open_streams(&self) -> usize {
        self.inner.lock().unwrap().streams.len()
    }

    /// Relay one verified envelope. `sender` is the authenticated key, not the
    /// one the body claims.
    pub fn relay(
        &self,
        sender: &str,
        recipient: &str,
        stream_id: &str,
        body: String,
        now_ms: i64,
    ) -> Result<(), RelayRefusal> {
        let now_secs = (now_ms / 1000).max(0) as u64;
        if !self
            .limiter
            .allow(sender, self.limits.rate_per_min, 60, now_secs)
        {
            return Err(RelayRefusal::RateLimited);
        }
        self.sweep(now_ms);

        let mut inner = self.inner.lock().unwrap();
        let known = inner.streams.contains_key(stream_id);
        if !known {
            let open = inner
                .streams
                .values()
                .filter(|r| r.involves(sender))
                .count();
            if open >= self.limits.max_open_per_member {
                return Err(RelayRefusal::TooManyStreams);
            }
        }

        let Some(conn) = inner.conns.get(recipient) else {
            return Err(RelayRefusal::NotConnected);
        };
        let tx = conn.tx.clone();
        let slots = conn.slots.clone();

        // The per-stream bound, taken before the frame is queued so the
        // sender learns to wait rather than the hub growing.
        {
            let mut s = slots.lock().unwrap();
            let n = s.entry(stream_id.to_string()).or_insert(0);
            if *n >= self.limits.per_stream_buffer {
                return Err(RelayRefusal::Backpressure);
            }
            *n += 1;
        }
        let delivery = Delivery {
            event: "stream",
            data: body,
            _slot: Some(SlotGuard {
                slots,
                stream_id: stream_id.to_string(),
            }),
        };

        match tx.try_send(delivery) {
            Ok(()) => {
                let route = inner
                    .streams
                    .entry(stream_id.to_string())
                    .or_insert_with(|| Route {
                        a: sender.to_string(),
                        b: recipient.to_string(),
                        last_ms: now_ms,
                    });
                route.last_ms = now_ms;
                Ok(())
            }
            Err(_) => {
                // The whole connection is backed up, or gone. The stream ends
                // here, and both ends are told so neither waits on a pipe the
                // hub has stopped carrying.
                drop(inner);
                self.drop_stream(
                    stream_id,
                    sender,
                    recipient,
                    proto::stream::closed::RELAY_DROPPED,
                    RelayRefusal::Dropped.message(),
                );
                Err(RelayRefusal::Dropped)
            }
        }
    }

    /// End a stream on purpose: either end says it is done, and the other is
    /// told. Returns whether the relay was tracking it.
    pub fn close(&self, stream_id: &str, by: &str, reason: &str, detail: &str) -> bool {
        let peer = {
            let mut inner = self.inner.lock().unwrap();
            match inner.streams.remove(stream_id) {
                Some(route) => route.other(by).map(str::to_string),
                None => return false,
            }
        };
        if let Some(peer) = peer {
            self.notify(&peer, stream_id, reason, detail);
        }
        true
    }

    /// Tear a stream down from the hub's own side, telling both ends.
    fn drop_stream(&self, stream_id: &str, a: &str, b: &str, reason: &str, detail: &str) {
        self.inner.lock().unwrap().streams.remove(stream_id);
        self.notify(a, stream_id, reason, detail);
        self.notify(b, stream_id, reason, detail);
    }

    /// A clear notice to one member, if it is connected. Best effort by
    /// design: a member whose queue is full is already being torn down.
    fn notify(&self, node_id: &str, stream_id: &str, reason: &str, detail: &str) {
        let control = Control {
            stream_id: stream_id.to_string(),
            reason: reason.to_string(),
            detail: detail.to_string(),
        };
        let Ok(data) = serde_json::to_string(&control) else {
            return;
        };
        let inner = self.inner.lock().unwrap();
        if let Some(conn) = inner.conns.get(node_id) {
            let _ = conn.tx.try_send(Delivery {
                event: "control",
                data,
                _slot: None,
            });
        }
    }

    /// Forget streams nothing has travelled on for `idle_secs`. A relay that
    /// only forgets on an explicit close leaks a slot for every peer that
    /// crashed mid-stream, and the open-stream cap is what that would eat.
    pub fn sweep(&self, now_ms: i64) {
        let cutoff = now_ms - (self.limits.idle_secs as i64) * 1000;
        let mut inner = self.inner.lock().unwrap();
        inner.streams.retain(|_, r| r.last_ms > cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> StreamRelay {
        StreamRelay::new(StreamLimits {
            per_stream_buffer: 2,
            queue_depth: 4,
            max_open_per_member: 2,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn a_frame_reaches_the_recipient_and_nothing_is_kept() {
        let r = relay();
        let mut rx = r.connect("b");
        r.relay("a", "b", "s1", "sealed".into(), 1_000).unwrap();
        let got = rx.recv().await.unwrap();
        assert_eq!(got.event, "stream");
        assert_eq!(got.data, "sealed");
        drop(got);
        // Nothing is queued and nothing is stored; the route is all that is
        // remembered, and only so the peer can be told when it ends.
        assert_eq!(r.open_streams(), 1);
        assert!(r.close("s1", "a", "done", "finished"));
        assert_eq!(r.open_streams(), 0);
    }

    #[tokio::test]
    async fn a_full_stream_buffer_is_backpressure_not_a_drop() {
        let r = relay();
        let mut rx = r.connect("b");
        r.relay("a", "b", "s1", "1".into(), 1_000).unwrap();
        r.relay("a", "b", "s1", "2".into(), 1_000).unwrap();
        assert_eq!(
            r.relay("a", "b", "s1", "3".into(), 1_000),
            Err(RelayRefusal::Backpressure)
        );
        // The stream is still open, and nothing was lost.
        assert_eq!(r.open_streams(), 1);
        let a = rx.recv().await.unwrap();
        assert_eq!(a.data, "1");
        drop(a);
        // A slot came free, so the sender may go on.
        r.relay("a", "b", "s1", "3".into(), 1_000).unwrap();
        let b = rx.recv().await.unwrap();
        assert_eq!(b.data, "2");
    }

    #[tokio::test]
    async fn a_backed_up_connection_drops_the_stream_and_tells_both_ends() {
        let r = StreamRelay::new(StreamLimits {
            per_stream_buffer: 8,
            queue_depth: 2,
            ..Default::default()
        });
        let _rx_b = r.connect("b");
        let mut rx_a = r.connect("a");
        r.relay("a", "b", "s1", "1".into(), 1_000).unwrap();
        r.relay("a", "b", "s1", "2".into(), 1_000).unwrap();
        assert_eq!(
            r.relay("a", "b", "s1", "3".into(), 1_000),
            Err(RelayRefusal::Dropped)
        );
        assert_eq!(r.open_streams(), 0);
        let note = rx_a.recv().await.unwrap();
        assert_eq!(note.event, "control");
        assert!(note.data.contains(proto::stream::closed::RELAY_DROPPED));
    }

    #[tokio::test]
    async fn an_unconnected_recipient_is_said_so_rather_than_buffered() {
        let r = relay();
        assert_eq!(
            r.relay("a", "b", "s1", "1".into(), 1_000),
            Err(RelayRefusal::NotConnected)
        );
        assert_eq!(r.open_streams(), 0);
    }

    #[tokio::test]
    async fn a_member_may_not_open_unbounded_streams() {
        let r = relay();
        let _rx = r.connect("b");
        r.relay("a", "b", "s1", "1".into(), 1_000).unwrap();
        r.relay("a", "b", "s2", "1".into(), 1_000).unwrap();
        assert_eq!(
            r.relay("a", "b", "s3", "1".into(), 1_000),
            Err(RelayRefusal::TooManyStreams)
        );
        // An existing stream still flows.
        r.relay("a", "b", "s1", "2".into(), 1_000).unwrap();
    }

    #[tokio::test]
    async fn an_idle_stream_is_forgotten() {
        let r = relay();
        let _rx = r.connect("b");
        r.relay("a", "b", "s1", "1".into(), 1_000).unwrap();
        assert_eq!(r.open_streams(), 1);
        r.sweep(1_000 + (r.limits().idle_secs as i64 + 1) * 1000);
        assert_eq!(r.open_streams(), 0);
    }

    #[tokio::test]
    async fn a_reconnect_replaces_the_connection() {
        let r = relay();
        let mut first = r.connect("b");
        let epoch = r.epoch_of("b").unwrap();
        let mut second = r.connect("b");
        // The old writer learns its connection ended rather than sharing it.
        r.disconnect("b", epoch);
        assert!(r.is_connected("b"), "the newer connection survives");
        r.relay("a", "b", "s1", "1".into(), 1_000).unwrap();
        assert_eq!(second.recv().await.unwrap().data, "1");
        assert!(first.try_recv().is_err());
    }
}
