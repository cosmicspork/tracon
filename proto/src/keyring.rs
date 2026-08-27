//! Channel keys as epochs. A channel's keyring holds every epoch key wrapped to
//! one node; rotation mints a new epoch, frames seal under the newest, old
//! frames keep opening. Epoch ids are opaque (random; genesis is all-zero) so
//! rings from independent sources merge by union. The epoch rides in the AEAD
//! associated data (`channel ‖ 0x1f ‖ epoch_id`), so the hub cannot re-label a
//! frame and a frame cannot be replayed under the wrong epoch.

use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use x25519_dalek::PublicKey;

use crate::envelope::{wrap_key, wrap_key_with_ephemeral, DataKey, EnvelopeError, WrappedKey};
use crate::keys::Identity;

pub const EPOCH_ID_LEN: usize = 16;
const GENESIS_EPOCH_ID: [u8; EPOCH_ID_LEN] = [0u8; EPOCH_ID_LEN];
const MAGIC: &[u8; 4] = b"trkr";
const FORMAT: u8 = 1;
/// Outside the channel-name charset, so `channel ‖ sep ‖ epoch` is unambiguous.
pub const AAD_SEP: u8 = 0x1f;

#[derive(Debug, thiserror::Error)]
pub enum KeyringError {
    #[error("malformed keyring bytes")]
    Format,
}

#[derive(Clone, Debug)]
pub struct KeyringEntry {
    id: [u8; EPOCH_ID_LEN],
    created_at: i64,
    wrapped: WrappedKey,
}

impl KeyringEntry {
    pub fn id(&self) -> [u8; EPOCH_ID_LEN] {
        self.id
    }
    pub fn id_hex(&self) -> String {
        hex::encode(self.id)
    }
    pub fn created_at(&self) -> i64 {
        self.created_at
    }
    pub fn is_genesis(&self) -> bool {
        self.id == GENESIS_EPOCH_ID
    }
}

/// The epoch keys of one channel, each wrapped to one recipient. Always holds
/// at least the genesis epoch.
#[derive(Clone, Debug)]
pub struct Keyring {
    entries: Vec<KeyringEntry>,
}

/// AAD for a frame on `channel` under `epoch`.
pub fn channel_aad(channel: &str, epoch: &[u8; EPOCH_ID_LEN]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(channel.len() + 1 + EPOCH_ID_LEN);
    aad.extend_from_slice(channel.as_bytes());
    aad.push(AAD_SEP);
    aad.extend_from_slice(epoch);
    aad
}

impl Keyring {
    pub fn genesis(recipient: &PublicKey, key: &DataKey) -> Self {
        Self {
            entries: vec![KeyringEntry {
                id: GENESIS_EPOCH_ID,
                created_at: 0,
                wrapped: wrap_key(recipient, key),
            }],
        }
    }

    /// Reproducible genesis for test vectors only.
    pub fn genesis_with(
        recipient: &PublicKey,
        key: &DataKey,
        ephemeral_secret: [u8; 32],
        nonce: [u8; 24],
    ) -> Self {
        Self {
            entries: vec![KeyringEntry {
                id: GENESIS_EPOCH_ID,
                created_at: 0,
                wrapped: wrap_key_with_ephemeral(recipient, key, ephemeral_secret, nonce),
            }],
        }
    }

    /// Canonical order: ascending `(created_at, id)`.
    pub fn entries(&self) -> Vec<&KeyringEntry> {
        let mut refs: Vec<&KeyringEntry> = self.entries.iter().collect();
        refs.sort_by_key(|e| (e.created_at, e.id));
        refs
    }

    pub fn newest(&self) -> &KeyringEntry {
        self.entries
            .iter()
            .max_by(|a, b| (a.created_at, a.id).cmp(&(b.created_at, b.id)))
            .expect("a keyring always holds the genesis epoch")
    }

    pub fn entry(&self, id: &[u8; EPOCH_ID_LEN]) -> Option<&KeyringEntry> {
        self.entries.iter().find(|e| &e.id == id)
    }

    /// Unwrap one epoch's key with the recipient identity this ring is wrapped to.
    pub fn key_for(&self, entry: &KeyringEntry, me: &Identity) -> Result<DataKey, EnvelopeError> {
        me.unwrap_key(&entry.wrapped)
    }

    pub fn rotate(&self, recipient: &PublicKey, created_at: i64) -> (Self, DataKey) {
        let key = DataKey::generate();
        let mut id = [0u8; EPOCH_ID_LEN];
        OsRng.fill_bytes(&mut id);
        (
            self.with_epoch(id, created_at, wrap_key(recipient, &key)),
            key,
        )
    }

    /// Reproducible rotation for test vectors only.
    pub fn rotate_with(
        &self,
        recipient: &PublicKey,
        key: &DataKey,
        id: [u8; EPOCH_ID_LEN],
        created_at: i64,
        ephemeral_secret: [u8; 32],
        nonce: [u8; 24],
    ) -> Self {
        self.with_epoch(
            id,
            created_at,
            wrap_key_with_ephemeral(recipient, key, ephemeral_secret, nonce),
        )
    }

    fn with_epoch(&self, id: [u8; EPOCH_ID_LEN], created_at: i64, wrapped: WrappedKey) -> Self {
        let mut entries = self.entries.clone();
        entries.push(KeyringEntry {
            id,
            created_at,
            wrapped,
        });
        Self { entries }
    }

    /// Union by epoch id. Commutative; on a shared id the lexicographically
    /// greater wrapped bytes win (both wrap the same key).
    pub fn merge(a: &Keyring, b: &Keyring) -> Keyring {
        let mut merged: Vec<KeyringEntry> = Vec::new();
        for e in a.entries.iter().chain(b.entries.iter()) {
            match merged.iter_mut().find(|m| m.id == e.id) {
                Some(m) => {
                    if e.wrapped.to_bytes() > m.wrapped.to_bytes() {
                        *m = e.clone();
                    }
                }
                None => merged.push(e.clone()),
            }
        }
        Keyring { entries: merged }
    }

    /// Re-wrap every epoch from `owner` to `grantee`: the ring a newly enrolled
    /// or still-trusted node receives in a key handoff.
    pub fn wrap_for(
        &self,
        owner: &Identity,
        grantee: &PublicKey,
    ) -> Result<Keyring, EnvelopeError> {
        let mut entries = Vec::with_capacity(self.entries.len());
        for e in self.entries() {
            let key = owner.unwrap_key(&e.wrapped)?;
            entries.push(KeyringEntry {
                id: e.id,
                created_at: e.created_at,
                wrapped: wrap_key(grantee, &key),
            });
        }
        Ok(Keyring { entries })
    }

    /// Reproducible re-wrap for test vectors only; one `(ephemeral, nonce)` per
    /// epoch in canonical order.
    pub fn wrap_for_with(
        &self,
        owner: &Identity,
        grantee: &PublicKey,
        ephemerals: &[([u8; 32], [u8; 24])],
    ) -> Result<Keyring, EnvelopeError> {
        let ordered = self.entries();
        assert_eq!(ordered.len(), ephemerals.len());
        let mut entries = Vec::with_capacity(ordered.len());
        for (e, &(eph, nonce)) in ordered.iter().zip(ephemerals) {
            let key = owner.unwrap_key(&e.wrapped)?;
            entries.push(KeyringEntry {
                id: e.id,
                created_at: e.created_at,
                wrapped: wrap_key_with_ephemeral(grantee, &key, eph, nonce),
            });
        }
        Ok(Keyring { entries })
    }

    /// `magic ‖ format ‖ count(u32) ‖ [ id(16) ‖ created_at(i64) ‖ len(u32) ‖ wrapped ]…`
    pub fn to_bytes(&self) -> Vec<u8> {
        let ordered = self.entries();
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(FORMAT);
        out.extend_from_slice(&(ordered.len() as u32).to_be_bytes());
        for e in ordered {
            out.extend_from_slice(&e.id);
            out.extend_from_slice(&e.created_at.to_be_bytes());
            let w = e.wrapped.to_bytes();
            out.extend_from_slice(&(w.len() as u32).to_be_bytes());
            out.extend_from_slice(&w);
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Keyring, KeyringError> {
        if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
            return Err(KeyringError::Format);
        }
        let mut cur = &bytes[MAGIC.len()..];
        if take(&mut cur, 1)?[0] != FORMAT {
            return Err(KeyringError::Format);
        }
        let count = u32::from_be_bytes(take(&mut cur, 4)?.try_into().unwrap());
        let mut entries = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let id: [u8; EPOCH_ID_LEN] = take(&mut cur, EPOCH_ID_LEN)?.try_into().unwrap();
            let created_at = i64::from_be_bytes(take(&mut cur, 8)?.try_into().unwrap());
            let len = u32::from_be_bytes(take(&mut cur, 4)?.try_into().unwrap()) as usize;
            let wrapped =
                WrappedKey::from_bytes(take(&mut cur, len)?).map_err(|_| KeyringError::Format)?;
            entries.push(KeyringEntry {
                id,
                created_at,
                wrapped,
            });
        }
        if !cur.is_empty() || entries.is_empty() {
            return Err(KeyringError::Format);
        }
        Ok(Keyring { entries })
    }
}

fn take<'a>(cur: &mut &'a [u8], n: usize) -> Result<&'a [u8], KeyringError> {
    if cur.len() < n {
        return Err(KeyringError::Format);
    }
    let (head, tail) = cur.split_at(n);
    *cur = tail;
    Ok(head)
}

/// Rides in JSON as hex of the container bytes.
impl Serialize for Keyring {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(self.to_bytes()))
    }
}

impl<'de> Deserialize<'de> for Keyring {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        Keyring::from_bytes(&bytes).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::key32;

    fn owner() -> Identity {
        Identity::from_seed(&[11u8; 32])
    }
    fn grantee() -> Identity {
        Identity::from_seed(&[12u8; 32])
    }

    #[test]
    fn genesis_rotate_merge_wrap_for() {
        let o = owner();
        let g = grantee();
        let k0 = DataKey::generate();
        let k0b = k0.to_bytes();
        let ring = Keyring::genesis(&o.x25519_public(), &k0);
        assert!(ring.newest().is_genesis());
        let parsed = Keyring::from_bytes(&ring.to_bytes()).unwrap();
        assert_eq!(parsed.key_for(parsed.newest(), &o).unwrap().to_bytes(), k0b);

        let (r1, k1) = ring.rotate(&o.x25519_public(), 1000);
        assert!(!r1.newest().is_genesis());
        assert_eq!(
            r1.key_for(r1.newest(), &o).unwrap().to_bytes(),
            k1.to_bytes()
        );
        // Genesis still there.
        assert!(r1.entry(&GENESIS_EPOCH_ID).is_some());

        // Two independent rotations merge to three epochs.
        let (r2, _) = ring.rotate(&o.x25519_public(), 2000);
        let m = Keyring::merge(&r1, &r2);
        assert_eq!(m.entries().len(), 3);
        assert_eq!(Keyring::merge(&r2, &r1).to_bytes(), m.to_bytes());

        // Handoff: grantee opens every epoch; owner cannot open the handed ring.
        let h = m.wrap_for(&o, &g.x25519_public()).unwrap();
        for e in h.entries() {
            assert!(h.key_for(e, &g).is_ok());
            assert!(h.key_for(e, &o).is_err());
        }
        let hb = Keyring::from_bytes(&h.to_bytes()).unwrap();
        assert_eq!(hb.to_bytes(), h.to_bytes());
    }

    #[test]
    fn rejects_bad_bytes() {
        assert!(Keyring::from_bytes(b"nope").is_err());
        let o = owner();
        let ring = Keyring::genesis(&o.x25519_public(), &DataKey::generate());
        let mut b = ring.to_bytes();
        b.push(0);
        assert!(Keyring::from_bytes(&b).is_err());
        b.truncate(b.len() - 10);
        assert!(Keyring::from_bytes(&b).is_err());
    }

    #[test]
    fn aad_layout() {
        let aad = channel_aad("work", &[1u8; 16]);
        assert_eq!(&aad[..4], b"work");
        assert_eq!(aad[4], AAD_SEP);
        assert_eq!(&aad[5..], &[1u8; 16]);
    }

    #[derive(serde::Deserialize)]
    struct VectorFile {
        contract_version: u32,
        owner_seed_hex: String,
        grantee_seed_hex: String,
        genesis_key_hex: String,
        genesis_ephemeral_hex: String,
        genesis_nonce_hex: String,
        rotated_key_hex: String,
        rotated_id_hex: String,
        rotated_created_at: i64,
        rotated_ephemeral_hex: String,
        rotated_nonce_hex: String,
        ring_hex: String,
        handoff_ephemerals: Vec<(String, String)>,
        handoff_hex: String,
    }
    const VECTORS: &str = include_str!("../../spec/vectors/keyring.json");

    #[test]
    fn matches_spec_vectors() {
        let f: VectorFile = serde_json::from_str(VECTORS).unwrap();
        assert_eq!(f.contract_version, crate::CONTRACT_VERSION);
        let a24 = |h: &str| -> [u8; 24] { hex::decode(h).unwrap().try_into().unwrap() };
        let a16 = |h: &str| -> [u8; 16] { hex::decode(h).unwrap().try_into().unwrap() };
        let o = Identity::from_seed(&key32(&f.owner_seed_hex).unwrap());
        let g = Identity::from_seed(&key32(&f.grantee_seed_hex).unwrap());
        let ring = Keyring::genesis_with(
            &o.x25519_public(),
            &DataKey::from_bytes(key32(&f.genesis_key_hex).unwrap()),
            key32(&f.genesis_ephemeral_hex).unwrap(),
            a24(&f.genesis_nonce_hex),
        )
        .rotate_with(
            &o.x25519_public(),
            &DataKey::from_bytes(key32(&f.rotated_key_hex).unwrap()),
            a16(&f.rotated_id_hex),
            f.rotated_created_at,
            key32(&f.rotated_ephemeral_hex).unwrap(),
            a24(&f.rotated_nonce_hex),
        );
        assert_eq!(hex::encode(ring.to_bytes()), f.ring_hex);
        let parsed = Keyring::from_bytes(&hex::decode(&f.ring_hex).unwrap()).unwrap();
        assert_eq!(parsed.newest().id_hex(), f.rotated_id_hex);
        assert_eq!(
            parsed.key_for(parsed.newest(), &o).unwrap().to_bytes(),
            key32(&f.rotated_key_hex).unwrap()
        );
        let eph: Vec<([u8; 32], [u8; 24])> = f
            .handoff_ephemerals
            .iter()
            .map(|(e, n)| (key32(e).unwrap(), a24(n)))
            .collect();
        let h = ring.wrap_for_with(&o, &g.x25519_public(), &eph).unwrap();
        assert_eq!(hex::encode(h.to_bytes()), f.handoff_hex);
        assert_eq!(
            h.key_for(h.newest(), &g).unwrap().to_bytes(),
            key32(&f.rotated_key_hex).unwrap()
        );
    }
}
