//! Sealing primitives: XChaCha20-Poly1305 under a 256-bit [`DataKey`], and ECIES
//! to an X25519 recipient ([`WrappedKey`] for a 32-byte key, [`SealedBox`] for an
//! arbitrary body). The HKDF label domain-separates the two ECIES uses and embeds
//! the contract major.

use chacha20poly1305::{
    aead::{Aead, Payload},
    Key, KeyInit, XChaCha20Poly1305, XNonce,
};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

pub const NONCE_LEN: usize = 24;
pub const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;

const WRAP_LABEL: &str = "wrap";
const BOX_LABEL: &str = "box";

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    /// Wrong key, wrong associated data, or tampered ciphertext. Opaque on
    /// purpose.
    #[error("authenticated decryption failed")]
    Aead,
    #[error("malformed envelope bytes")]
    Format,
}

/// A symmetric key. Zeroizes on drop; not `Clone`/`Debug`.
pub struct DataKey([u8; KEY_LEN]);

impl Drop for DataKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl DataKey {
    pub fn generate() -> Self {
        let mut key = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut key);
        Self(key)
    }

    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Raw bytes; only for tests and vector generation.
    pub fn to_bytes(&self) -> [u8; KEY_LEN] {
        self.0
    }

    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Sealed {
        self.seal_with_nonce(random_nonce(), plaintext, aad)
    }

    /// Reproducible sealing for test vectors only; a nonce must never repeat.
    pub fn seal_with_nonce(&self, nonce: [u8; NONCE_LEN], plaintext: &[u8], aad: &[u8]) -> Sealed {
        Sealed {
            nonce,
            ciphertext: aead_seal(&self.0, &nonce, plaintext, aad),
        }
    }

    pub fn open(&self, sealed: &Sealed, aad: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
        aead_open(&self.0, &sealed.nonce, &sealed.ciphertext, aad)
    }
}

/// `nonce(24) ‖ ciphertext+tag`.
#[derive(Clone, Debug)]
pub struct Sealed {
    nonce: [u8; NONCE_LEN],
    ciphertext: Vec<u8>,
}

impl Sealed {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(NONCE_LEN + self.ciphertext.len());
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.ciphertext);
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        if bytes.len() < NONCE_LEN + TAG_LEN {
            return Err(EnvelopeError::Format);
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&bytes[..NONCE_LEN]);
        Ok(Self {
            nonce,
            ciphertext: bytes[NONCE_LEN..].to_vec(),
        })
    }
}

/// A [`DataKey`] wrapped to an X25519 recipient: `ephemeral_public(32) ‖ sealed`.
#[derive(Clone, Debug)]
pub struct WrappedKey {
    ephemeral_public: [u8; KEY_LEN],
    sealed: Sealed,
}

impl WrappedKey {
    pub fn to_bytes(&self) -> Vec<u8> {
        ecies_bytes(&self.ephemeral_public, &self.sealed)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        let (ephemeral_public, sealed) = ecies_parse(bytes)?;
        Ok(Self {
            ephemeral_public,
            sealed,
        })
    }

    pub(crate) fn open(
        &self,
        secret: &StaticSecret,
        recipient_public: &PublicKey,
    ) -> Result<DataKey, EnvelopeError> {
        let shared = secret.diffie_hellman(&PublicKey::from(self.ephemeral_public));
        let mut k = derive_box_key(
            shared.as_bytes(),
            &self.ephemeral_public,
            recipient_public.as_bytes(),
            WRAP_LABEL,
        );
        let plaintext = aead_open(&k, &self.sealed.nonce, &self.sealed.ciphertext, &[]);
        k.zeroize();
        let bytes: [u8; KEY_LEN] = plaintext?.try_into().map_err(|_| EnvelopeError::Format)?;
        Ok(DataKey::from_bytes(bytes))
    }
}

pub fn wrap_key(recipient: &PublicKey, key: &DataKey) -> WrappedKey {
    let mut eph = [0u8; KEY_LEN];
    OsRng.fill_bytes(&mut eph);
    let w = wrap_key_with_ephemeral(recipient, key, eph, random_nonce());
    eph.zeroize();
    w
}

/// Reproducible wrapping for test vectors only.
pub fn wrap_key_with_ephemeral(
    recipient: &PublicKey,
    key: &DataKey,
    ephemeral_secret: [u8; KEY_LEN],
    nonce: [u8; NONCE_LEN],
) -> WrappedKey {
    let eph_secret = StaticSecret::from(ephemeral_secret);
    let eph_public = PublicKey::from(&eph_secret);
    let shared = eph_secret.diffie_hellman(recipient);
    let mut k = derive_box_key(
        shared.as_bytes(),
        eph_public.as_bytes(),
        recipient.as_bytes(),
        WRAP_LABEL,
    );
    let ciphertext = aead_seal(&k, &nonce, &key.0, &[]);
    k.zeroize();
    WrappedKey {
        ephemeral_public: eph_public.to_bytes(),
        sealed: Sealed { nonce, ciphertext },
    }
}

/// An arbitrary body sealed to an X25519 recipient; same layout as
/// [`WrappedKey`], different HKDF label, so one can never open as the other.
#[derive(Clone, Debug)]
pub struct SealedBox {
    ephemeral_public: [u8; KEY_LEN],
    sealed: Sealed,
}

impl SealedBox {
    pub fn to_bytes(&self) -> Vec<u8> {
        ecies_bytes(&self.ephemeral_public, &self.sealed)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        let (ephemeral_public, sealed) = ecies_parse(bytes)?;
        Ok(Self {
            ephemeral_public,
            sealed,
        })
    }

    pub(crate) fn open(
        &self,
        secret: &StaticSecret,
        recipient_public: &PublicKey,
        aad: &[u8],
    ) -> Result<Vec<u8>, EnvelopeError> {
        let shared = secret.diffie_hellman(&PublicKey::from(self.ephemeral_public));
        let mut k = derive_box_key(
            shared.as_bytes(),
            &self.ephemeral_public,
            recipient_public.as_bytes(),
            BOX_LABEL,
        );
        let plaintext = aead_open(&k, &self.sealed.nonce, &self.sealed.ciphertext, aad);
        k.zeroize();
        plaintext
    }
}

pub fn seal_to(recipient: &PublicKey, plaintext: &[u8], aad: &[u8]) -> SealedBox {
    let mut eph = [0u8; KEY_LEN];
    OsRng.fill_bytes(&mut eph);
    let s = seal_to_with_ephemeral(recipient, plaintext, aad, eph, random_nonce());
    eph.zeroize();
    s
}

/// Reproducible sealing for test vectors only.
pub fn seal_to_with_ephemeral(
    recipient: &PublicKey,
    plaintext: &[u8],
    aad: &[u8],
    ephemeral_secret: [u8; KEY_LEN],
    nonce: [u8; NONCE_LEN],
) -> SealedBox {
    let eph_secret = StaticSecret::from(ephemeral_secret);
    let eph_public = PublicKey::from(&eph_secret);
    let shared = eph_secret.diffie_hellman(recipient);
    let mut k = derive_box_key(
        shared.as_bytes(),
        eph_public.as_bytes(),
        recipient.as_bytes(),
        BOX_LABEL,
    );
    let ciphertext = aead_seal(&k, &nonce, plaintext, aad);
    k.zeroize();
    SealedBox {
        ephemeral_public: eph_public.to_bytes(),
        sealed: Sealed { nonce, ciphertext },
    }
}

fn ecies_bytes(eph: &[u8; KEY_LEN], sealed: &Sealed) -> Vec<u8> {
    let s = sealed.to_bytes();
    let mut out = Vec::with_capacity(KEY_LEN + s.len());
    out.extend_from_slice(eph);
    out.extend_from_slice(&s);
    out
}

fn ecies_parse(bytes: &[u8]) -> Result<([u8; KEY_LEN], Sealed), EnvelopeError> {
    if bytes.len() < KEY_LEN {
        return Err(EnvelopeError::Format);
    }
    let mut eph = [0u8; KEY_LEN];
    eph.copy_from_slice(&bytes[..KEY_LEN]);
    Ok((eph, Sealed::from_bytes(&bytes[KEY_LEN..])?))
}

/// HKDF-SHA256 over the DH secret, salted with both public keys so the key is
/// pinned to this exchange, labelled with the operation and contract major.
fn derive_box_key(
    shared: &[u8],
    ephemeral_public: &[u8; KEY_LEN],
    recipient_public: &[u8; KEY_LEN],
    operation: &str,
) -> [u8; KEY_LEN] {
    let mut salt = [0u8; KEY_LEN * 2];
    salt[..KEY_LEN].copy_from_slice(ephemeral_public);
    salt[KEY_LEN..].copy_from_slice(recipient_public);
    let hk = Hkdf::<Sha256>::new(Some(&salt), shared);
    let mut okm = [0u8; KEY_LEN];
    hk.expand(crate::version_label(operation).as_bytes(), &mut okm)
        .expect("HKDF expand of 32 bytes is always within bounds");
    okm
}

fn aead_seal(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
    aad: &[u8],
) -> Vec<u8> {
    XChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("sealing an in-memory buffer cannot fail")
}

fn aead_open(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, EnvelopeError> {
    XChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| EnvelopeError::Aead)
}

fn random_nonce() -> [u8; NONCE_LEN] {
    let mut n = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut n);
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{key32, Identity};

    fn arr24(h: &str) -> [u8; 24] {
        hex::decode(h).unwrap().try_into().unwrap()
    }

    #[test]
    fn seal_open_round_trip_and_rejections() {
        let key = DataKey::generate();
        let sealed = key.seal(b"frame body", b"personal\x1fepoch");
        assert_eq!(
            key.open(&sealed, b"personal\x1fepoch").unwrap(),
            b"frame body"
        );
        assert!(matches!(
            key.open(&sealed, b"work\x1fepoch"),
            Err(EnvelopeError::Aead)
        ));
        assert!(matches!(
            DataKey::generate().open(&sealed, b"personal\x1fepoch"),
            Err(EnvelopeError::Aead)
        ));
        let mut t = sealed.clone();
        t.ciphertext[0] ^= 1;
        assert!(matches!(
            key.open(&t, b"personal\x1fepoch"),
            Err(EnvelopeError::Aead)
        ));
        assert!(matches!(
            Sealed::from_bytes(&[0u8; 8]),
            Err(EnvelopeError::Format)
        ));
    }

    #[test]
    fn wrap_and_box_round_trip_and_domain_separation() {
        let me = Identity::from_seed(&[9u8; 32]);
        let other = Identity::from_seed(&[8u8; 32]);
        let key = DataKey::generate();
        let kb = key.to_bytes();
        let w = WrappedKey::from_bytes(&wrap_key(&me.x25519_public(), &key).to_bytes()).unwrap();
        assert_eq!(me.unwrap_key(&w).unwrap().to_bytes(), kb);
        assert!(matches!(other.unwrap_key(&w), Err(EnvelopeError::Aead)));

        let b =
            SealedBox::from_bytes(&seal_to(&me.x25519_public(), b"hi", b"aad").to_bytes()).unwrap();
        assert_eq!(me.open_sealed_box(&b, b"aad").unwrap(), b"hi");
        assert!(matches!(
            me.open_sealed_box(&b, b"bad"),
            Err(EnvelopeError::Aead)
        ));

        // A 32-byte box must not open as a wrapped key: different labels.
        let boxed = seal_to(&me.x25519_public(), &[7u8; 32], b"");
        let as_wrapped = WrappedKey::from_bytes(&boxed.to_bytes()).unwrap();
        assert!(matches!(
            me.unwrap_key(&as_wrapped),
            Err(EnvelopeError::Aead)
        ));
    }

    #[derive(serde::Deserialize)]
    struct VectorFile {
        contract_version: u32,
        sealing: Vec<SealVector>,
        wrapping: Vec<WrapVector>,
        boxing: Vec<BoxVector>,
    }
    #[derive(serde::Deserialize)]
    struct SealVector {
        key_hex: String,
        nonce_hex: String,
        aad_hex: String,
        plaintext_hex: String,
        sealed_hex: String,
    }
    #[derive(serde::Deserialize)]
    struct WrapVector {
        recipient_seed_hex: String,
        ephemeral_secret_hex: String,
        nonce_hex: String,
        data_key_hex: String,
        wrapped_hex: String,
    }
    #[derive(serde::Deserialize)]
    struct BoxVector {
        recipient_seed_hex: String,
        ephemeral_secret_hex: String,
        nonce_hex: String,
        aad_hex: String,
        plaintext_hex: String,
        boxed_hex: String,
    }

    const VECTORS: &str = include_str!("../../spec/vectors/envelope.json");

    #[test]
    fn matches_spec_vectors() {
        let f: VectorFile = serde_json::from_str(VECTORS).unwrap();
        assert_eq!(f.contract_version, crate::CONTRACT_VERSION);
        for v in &f.sealing {
            let key = DataKey::from_bytes(key32(&v.key_hex).unwrap());
            let pt = hex::decode(&v.plaintext_hex).unwrap();
            let aad = hex::decode(&v.aad_hex).unwrap();
            let sealed = key.seal_with_nonce(arr24(&v.nonce_hex), &pt, &aad);
            assert_eq!(hex::encode(sealed.to_bytes()), v.sealed_hex);
            let parsed = Sealed::from_bytes(&hex::decode(&v.sealed_hex).unwrap()).unwrap();
            assert_eq!(key.open(&parsed, &aad).unwrap(), pt);
        }
        for v in &f.wrapping {
            let r = Identity::from_seed(&key32(&v.recipient_seed_hex).unwrap());
            let dk = key32(&v.data_key_hex).unwrap();
            let w = wrap_key_with_ephemeral(
                &r.x25519_public(),
                &DataKey::from_bytes(dk),
                key32(&v.ephemeral_secret_hex).unwrap(),
                arr24(&v.nonce_hex),
            );
            assert_eq!(hex::encode(w.to_bytes()), v.wrapped_hex);
            let parsed = WrappedKey::from_bytes(&hex::decode(&v.wrapped_hex).unwrap()).unwrap();
            assert_eq!(r.unwrap_key(&parsed).unwrap().to_bytes(), dk);
        }
        for v in &f.boxing {
            let r = Identity::from_seed(&key32(&v.recipient_seed_hex).unwrap());
            let pt = hex::decode(&v.plaintext_hex).unwrap();
            let aad = hex::decode(&v.aad_hex).unwrap();
            let b = seal_to_with_ephemeral(
                &r.x25519_public(),
                &pt,
                &aad,
                key32(&v.ephemeral_secret_hex).unwrap(),
                arr24(&v.nonce_hex),
            );
            assert_eq!(hex::encode(b.to_bytes()), v.boxed_hex);
            let parsed = SealedBox::from_bytes(&hex::decode(&v.boxed_hex).unwrap()).unwrap();
            assert_eq!(r.open_sealed_box(&parsed, &aad).unwrap(), pt);
        }
    }
}
