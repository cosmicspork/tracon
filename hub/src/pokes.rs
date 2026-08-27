//! Payload-free "go pull" hints to a node's open SSE streams. Lossy by design:
//! the cursor pull is the source of truth, so a dropped poke costs a node
//! nothing but latency until its next poll.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::broadcast;

const POKE_BUFFER: usize = 16;

/// Fan-out of pokes per identity. An absent key means nobody is listening.
#[derive(Default)]
pub struct PokeHub {
    channels: Mutex<HashMap<[u8; 32], broadcast::Sender<()>>>,
}

impl PokeHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe before returning the SSE response so a poke racing the
    /// handshake is not lost.
    pub fn subscribe(&self, identity: &[u8; 32]) -> broadcast::Receiver<()> {
        self.channels
            .lock()
            .unwrap()
            .entry(*identity)
            .or_insert_with(|| broadcast::channel(POKE_BUFFER).0)
            .subscribe()
    }

    /// Best-effort; reclaims the slot when the last stream has gone.
    pub fn poke(&self, identity: &[u8; 32]) {
        let mut channels = self.channels.lock().unwrap();
        if let Some(tx) = channels.get(identity) {
            if tx.send(()).is_err() {
                channels.remove(identity);
            }
        }
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
        assert!(a.recv().await.is_ok());
        assert!(b.try_recv().is_err());
        drop(a);
        hub.poke(&[1; 32]);
        assert_eq!(hub.channels.lock().unwrap().len(), 1);
        hub.poke(&[3; 32]);
    }
}
