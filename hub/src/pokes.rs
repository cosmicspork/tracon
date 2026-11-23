//! Payload-free hints to a node's open SSE streams, and the presence those
//! streams imply. Lossy by design: the cursor pull and the member list are the
//! source of truth, so a dropped poke costs a node nothing but latency until
//! its next poll.
//!
//! Presence is what the hub sees for itself, never what a node claims: a
//! member is connected while it holds at least one `/v0/events` stream, and
//! its last-seen time is its last authenticated request as a member. When a
//! member's connected state changes, every member is poked with
//! [`Poke::Members`] so it reads the member list again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

const POKE_BUFFER: usize = 16;

/// What a poke asks the node to read again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poke {
    /// A frame arrived on one of its channels.
    Frames,
    /// A member connected or disconnected.
    Members,
}

/// A member's presence as the hub sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Presence {
    /// Open `/v0/events` streams.
    pub streams: usize,
    /// The member's last authenticated request, hub clock.
    pub last_seen_ms: Option<i64>,
}

impl Presence {
    pub fn connected(&self) -> bool {
        self.streams > 0
    }
}

/// Fan-out of pokes per identity. An absent key means nobody is listening.
#[derive(Default)]
pub struct PokeHub {
    channels: Mutex<HashMap<[u8; 32], broadcast::Sender<Poke>>>,
    presence: Mutex<HashMap<[u8; 32], Presence>>,
}

/// One open stream's hold on its member's presence; dropping it (the stream
/// closed) releases it.
pub struct StreamGuard {
    hub: Arc<PokeHub>,
    identity: [u8; 32],
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.hub.stream_closed(&self.identity);
    }
}

impl PokeHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe before returning the SSE response so a poke racing the
    /// handshake is not lost.
    pub fn subscribe(&self, identity: &[u8; 32]) -> broadcast::Receiver<Poke> {
        self.channels
            .lock()
            .unwrap()
            .entry(*identity)
            .or_insert_with(|| broadcast::channel(POKE_BUFFER).0)
            .subscribe()
    }

    /// Subscribe a member's event stream and count it towards its presence.
    /// The first stream of a member makes it connected and pokes every member
    /// (this one too, as it is subscribed first) to read the list again.
    pub fn open_stream(
        self: &Arc<Self>,
        identity: &[u8; 32],
        now_ms: i64,
    ) -> (broadcast::Receiver<Poke>, StreamGuard) {
        let rx = self.subscribe(identity);
        let first = {
            let mut presence = self.presence.lock().unwrap();
            let p = presence.entry(*identity).or_default();
            p.streams += 1;
            p.last_seen_ms = Some(now_ms);
            p.streams == 1
        };
        if first {
            self.poke_all(Poke::Members);
        }
        (
            rx,
            StreamGuard {
                hub: self.clone(),
                identity: *identity,
            },
        )
    }

    fn stream_closed(&self, identity: &[u8; 32]) {
        let last = {
            let mut presence = self.presence.lock().unwrap();
            match presence.get_mut(identity) {
                Some(p) if p.streams > 0 => {
                    p.streams -= 1;
                    p.streams == 0
                }
                _ => false,
            }
        };
        if last {
            self.poke_all(Poke::Members);
        }
    }

    /// A member made an authenticated request.
    pub fn seen(&self, identity: &[u8; 32], now_ms: i64) {
        self.presence
            .lock()
            .unwrap()
            .entry(*identity)
            .or_default()
            .last_seen_ms = Some(now_ms);
    }

    pub fn presence_of(&self, identity: &[u8; 32]) -> Presence {
        self.presence
            .lock()
            .unwrap()
            .get(identity)
            .copied()
            .unwrap_or_default()
    }

    /// Best-effort; reclaims the slot when the last stream has gone.
    pub fn poke(&self, identity: &[u8; 32]) {
        let mut channels = self.channels.lock().unwrap();
        if let Some(tx) = channels.get(identity) {
            if tx.send(Poke::Frames).is_err() {
                channels.remove(identity);
            }
        }
    }

    fn poke_all(&self, poke: Poke) {
        self.channels
            .lock()
            .unwrap()
            .retain(|_, tx| tx.send(poke).is_ok());
    }

    pub fn live_streams(&self) -> usize {
        self.channels
            .lock()
            .unwrap()
            .values()
            .map(|tx| tx.receiver_count())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn poke_reaches_subscriber_and_isolates_identities() {
        let hub = PokeHub::new();
        let mut a = hub.subscribe(&[1; 32]);
        let mut b = hub.subscribe(&[2; 32]);
        hub.poke(&[1; 32]);
        assert_eq!(a.recv().await.unwrap(), Poke::Frames);
        assert!(b.try_recv().is_err());
        drop(a);
        hub.poke(&[1; 32]);
        assert_eq!(hub.channels.lock().unwrap().len(), 1);
        hub.poke(&[3; 32]);
    }

    #[tokio::test]
    async fn presence_follows_open_streams_and_pokes_members_on_change() {
        let hub = Arc::new(PokeHub::new());
        let mut watcher = hub.subscribe(&[9; 32]);
        assert!(!hub.presence_of(&[1; 32]).connected());

        let (_rx1, g1) = hub.open_stream(&[1; 32], 10);
        assert!(hub.presence_of(&[1; 32]).connected());
        assert_eq!(hub.presence_of(&[1; 32]).last_seen_ms, Some(10));
        assert_eq!(watcher.recv().await.unwrap(), Poke::Members);

        // A second stream of the same member changes nothing others see.
        let (_rx2, g2) = hub.open_stream(&[1; 32], 20);
        assert!(watcher.try_recv().is_err());
        drop(g1);
        assert!(hub.presence_of(&[1; 32]).connected());
        assert!(watcher.try_recv().is_err());

        // The last one closing disconnects it, and says so.
        drop(g2);
        assert!(!hub.presence_of(&[1; 32]).connected());
        assert_eq!(watcher.recv().await.unwrap(), Poke::Members);

        hub.seen(&[1; 32], 30);
        assert_eq!(hub.presence_of(&[1; 32]).last_seen_ms, Some(30));
    }
}
