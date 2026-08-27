//! Replay guard: the signed descriptor binds method, path, body hash, and
//! timestamp, and Ed25519 is deterministic, so a byte-for-byte replay carries
//! the identical signature. Remembering signatures until their timestamp ages
//! out of the freshness window closes the gap the window alone leaves open. No
//! wire change: the signature is the nonce.
//!
//! In memory only. A restart clears the window, briefly reopening replay for a
//! signature whose timestamp has not yet aged out; that keeps the hub a keyless
//! single binary with no durable auth state, which is worth more.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct NonceStore {
    seen: Mutex<HashMap<[u8; 64], u64>>,
}

impl NonceStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` if the signature is new (accept); `false` if seen within the
    /// window (replay). `now` drives an opportunistic prune.
    pub fn check_and_remember(&self, signature: &[u8; 64], expires_at: u64, now: u64) -> bool {
        let mut seen = self.seen.lock().unwrap();
        seen.retain(|_, exp| *exp > now);
        match seen.entry(*signature) {
            Entry::Occupied(_) => false,
            Entry::Vacant(slot) => {
                slot.insert(expires_at);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_once_then_rejects_until_aged_out() {
        let s = NonceStore::new();
        assert!(s.check_and_remember(&[1; 64], 1300, 1000));
        assert!(!s.check_and_remember(&[1; 64], 1300, 1000));
        assert!(s.check_and_remember(&[2; 64], 1300, 1000));
        assert!(s.check_and_remember(&[1; 64], 2000, 1400));
    }
}
