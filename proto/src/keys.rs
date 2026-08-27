//! Node identity: one 32-byte seed derives an X25519 keypair (sealing) and an
//! Ed25519 keypair (signing, and the node id). Not a BIP39 phrase: a node's
//! identity is disposable, and losing it is recovered by re-enrolling.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::envelope::{DataKey, EnvelopeError, SealedBox, WrappedKey};

/// Length of the identity seed.
pub const SEED_LEN: usize = 32;

/// A node's keys. Not `Clone`/`Debug`: secret material should not be casually
/// copied or logged. Secret halves zeroize on drop.
pub struct Identity {
    encryption: StaticSecret,
    signing: SigningKey,
}

impl Identity {
    /// Derive from a seed. HKDF-SHA256 with version-tagged labels, one per key.
    pub fn from_seed(seed: &[u8; SEED_LEN]) -> Self {
        let hk = Hkdf::<Sha256>::new(None, seed);
        Self {
            encryption: StaticSecret::from(expand(&hk, "x25519")),
            signing: SigningKey::from_bytes(&expand(&hk, "ed25519")),
        }
    }

    /// A fresh identity from the OS RNG. Returns the seed so the caller can
    /// persist it.
    pub fn generate() -> ([u8; SEED_LEN], Self) {
        let mut seed = [0u8; SEED_LEN];
        OsRng.fill_bytes(&mut seed);
        let id = Self::from_seed(&seed);
        (seed, id)
    }

    pub fn x25519_public(&self) -> PublicKey {
        PublicKey::from(&self.encryption)
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    /// The node id used everywhere: lowercase hex of the Ed25519 public key.
    pub fn node_id(&self) -> String {
        hex::encode(self.verifying_key().to_bytes())
    }

    /// Lowercase hex of the X25519 public key, as carried in enrollment and
    /// member records.
    pub fn x25519_hex(&self) -> String {
        hex::encode(self.x25519_public().as_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        self.signing.sign(message)
    }

    /// Unwrap a key wrapped to this identity.
    pub fn unwrap_key(&self, wrapped: &WrappedKey) -> Result<DataKey, EnvelopeError> {
        wrapped.open(&self.encryption, &self.x25519_public())
    }

    /// Open a box sealed to this identity.
    pub fn open_sealed_box(
        &self,
        sealed: &SealedBox,
        aad: &[u8],
    ) -> Result<Vec<u8>, EnvelopeError> {
        sealed.open(&self.encryption, &self.x25519_public(), aad)
    }
}

/// Verify an Ed25519 signature; a free function so the hub never holds an
/// [`Identity`].
pub fn verify(verifying_key: &VerifyingKey, message: &[u8], signature: &Signature) -> bool {
    verifying_key.verify(message, signature).is_ok()
}

/// Parse a 32-byte hex public key.
pub fn key32(hex_str: &str) -> Option<[u8; 32]> {
    hex::decode(hex_str).ok()?.try_into().ok()
}

fn expand(hk: &Hkdf<Sha256>, label: &str) -> [u8; 32] {
    let mut okm = [0u8; 32];
    hk.expand(crate::version_label(label).as_bytes(), &mut okm)
        .expect("HKDF expand of 32 bytes is always within bounds");
    okm
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct VectorFile {
        contract_version: u32,
        vectors: Vec<Vector>,
    }
    #[derive(Deserialize)]
    struct Vector {
        seed_hex: String,
        x25519_public_hex: String,
        ed25519_public_hex: String,
        node_id: String,
    }

    const VECTORS: &str = include_str!("../../spec/vectors/key-derivation.json");

    #[test]
    fn matches_spec_vectors() {
        let file: VectorFile = serde_json::from_str(VECTORS).unwrap();
        assert_eq!(file.contract_version, crate::CONTRACT_VERSION);
        for v in &file.vectors {
            let id = Identity::from_seed(&key32(&v.seed_hex).unwrap());
            assert_eq!(id.x25519_hex(), v.x25519_public_hex);
            assert_eq!(
                hex::encode(id.verifying_key().to_bytes()),
                v.ed25519_public_hex
            );
            assert_eq!(id.node_id(), v.node_id);
        }
    }

    #[test]
    fn derivation_is_deterministic_and_seed_specific() {
        let a = Identity::from_seed(&[1u8; 32]);
        let b = Identity::from_seed(&[1u8; 32]);
        let c = Identity::from_seed(&[2u8; 32]);
        assert_eq!(a.node_id(), b.node_id());
        assert_ne!(a.node_id(), c.node_id());
        assert_ne!(a.x25519_hex(), a.node_id());
    }

    #[test]
    fn sign_verify() {
        let id = Identity::from_seed(&[3u8; 32]);
        let sig = id.sign(b"hello");
        assert!(verify(&id.verifying_key(), b"hello", &sig));
        assert!(!verify(&id.verifying_key(), b"hellp", &sig));
    }
}
