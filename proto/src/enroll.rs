//! Enrollment: the one public write on the hub. An enrolled node opens a slot
//! under a short-lived code; the new node fills it with its public keys and a
//! name; the enrolled node fetches, the operator compares fingerprints, and the
//! new node is admitted. Nothing in the slot is secret, so the hub stores it in
//! the clear; the fingerprint comparison is what defeats a hub that substitutes
//! keys.
//!
//! The fingerprint covers the node id alone, so the two keys in a slot must be
//! tied together by something other than the operator's eyes: the filler signs
//! its sealing key with its signing key ([`binding_bytes`]) and both ends check
//! that proof. Without it the pair is two unrelated strings, and whoever can
//! write the slot — the hub, or anyone who learns the code — can keep the node
//! id the operator compares and substitute a sealing key it holds, which is the
//! key every channel keyring would then be wrapped to.

use ed25519_dalek::{Signature, VerifyingKey};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::keys::{key32, verify, Identity};

/// What the new node posts to `POST /v0/enroll/{code}`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollRequest {
    /// Ed25519 public key, hex: the node id.
    pub node_id: String,
    /// X25519 public key, hex: where its keyrings get wrapped to.
    pub x25519_pub: String,
    pub name: String,
    pub contract: u32,
    /// Free text the inviter shows beside the name (arch, harness version).
    #[serde(default)]
    pub facts: String,
    /// Ed25519 signature by `node_id` over [`binding_bytes`], hex: the proof
    /// that `x25519_pub` is this node's own. Defaulted so a request from
    /// before the proof existed parses and is refused by name, rather than
    /// failing as malformed JSON.
    #[serde(default)]
    pub binding_sig: String,
}

impl EnrollRequest {
    /// The request a node posts for itself, with its key binding signed.
    pub fn signed(identity: &Identity, name: &str, facts: &str) -> Self {
        Self {
            node_id: identity.node_id(),
            x25519_pub: identity.x25519_hex(),
            name: name.to_string(),
            contract: crate::CONTRACT_VERSION,
            facts: facts.to_string(),
            binding_sig: sign_binding(identity),
        }
    }

    /// Is the sealing key in this request signed by the node id in it?
    pub fn binding_ok(&self) -> bool {
        verify_binding(&self.node_id, &self.x25519_pub, &self.binding_sig)
    }
}

/// The bytes a node signs to bind its sealing key to its signing key. Neither
/// name nor code is covered: this is a standing statement about two keys, so
/// the same proof travels with the member record the hub keeps.
pub fn binding_bytes(node_id: &[u8; 32], x25519_pub: &[u8; 32]) -> Vec<u8> {
    let label = crate::version_label("enroll-binding");
    let mut out = Vec::with_capacity(label.len() + 64);
    out.extend_from_slice(label.as_bytes());
    out.extend_from_slice(node_id);
    out.extend_from_slice(x25519_pub);
    out
}

/// This identity's proof that its sealing key belongs to its node id, hex.
pub fn sign_binding(identity: &Identity) -> String {
    let bytes = binding_bytes(
        &identity.verifying_key().to_bytes(),
        identity.x25519_public().as_bytes(),
    );
    hex::encode(identity.sign(&bytes).to_bytes())
}

/// Does `sig_hex` prove that `x25519_pub` belongs to `node_id`? Hex in, so a
/// slot or a member record can be checked exactly as it arrives.
pub fn verify_binding(node_id: &str, x25519_pub: &str, sig_hex: &str) -> bool {
    let (Some(id), Some(x)) = (key32(node_id), key32(x25519_pub)) else {
        return false;
    };
    let Ok(sig) = hex::decode(sig_hex) else {
        return false;
    };
    let Ok(sig) = <[u8; 64]>::try_from(sig.as_slice()) else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&id) else {
        return false;
    };
    verify(&vk, &binding_bytes(&id, &x), &Signature::from_bytes(&sig))
}

/// Crockford base32 without the ambiguous letters; 8 characters = 40 bits.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
pub const CODE_LEN: usize = 8;

/// A fresh enrollment code.
pub fn new_code() -> String {
    let mut bytes = [0u8; CODE_LEN];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| ALPHABET[(b % 32) as usize] as char)
        .collect()
}

/// Canonical form of a code as typed by a human: uppercase, separators dropped,
/// the Crockford confusables folded (`O`→`0`, `I`/`L`→`1`). `None` if the result
/// is not exactly [`CODE_LEN`] alphabet characters.
pub fn normalize_code(input: &str) -> Option<String> {
    let mut out = String::with_capacity(CODE_LEN);
    for c in input.chars() {
        let c = match c.to_ascii_uppercase() {
            '-' | '·' | ' ' | '.' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        if !ALPHABET.contains(&(c as u8)) {
            return None;
        }
        out.push(c);
    }
    (out.len() == CODE_LEN).then_some(out)
}

/// Human-comparable fingerprint of a node id: the first 16 hex characters of
/// `SHA256(ed25519 public key)` in groups of four. Shown on both ends of an
/// enrollment.
pub fn fingerprint(ed25519_public: &[u8; 32]) -> String {
    let h = hex::encode(Sha256::digest(ed25519_public));
    h.as_bytes()[..16]
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fingerprint from a hex node id; `None` if the id is malformed.
pub fn fingerprint_hex(node_id: &str) -> Option<String> {
    crate::keys::key32(node_id).map(|k| fingerprint(&k))
}

/// The URL fragment form an inviter prints: `{hub}/#enroll={code}`. The fragment
/// never reaches a server.
pub fn invite_url(hub_url: &str, code: &str) -> String {
    format!("{}/#enroll={}", hub_url.trim_end_matches('/'), code)
}

/// Parse an invite URL (or a bare code) into `(hub_url, code)`.
pub fn parse_invite(input: &str) -> Option<(Option<String>, String)> {
    if let Some((base, frag)) = input.split_once("/#enroll=") {
        let code = normalize_code(frag)?;
        return Some((Some(base.to_string()), code));
    }
    normalize_code(input).map(|c| (None, c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_valid_and_normalize() {
        for _ in 0..100 {
            let c = new_code();
            assert_eq!(c.len(), CODE_LEN);
            assert_eq!(normalize_code(&c).unwrap(), c);
        }
        assert_eq!(normalize_code("7kq4·m2xa").unwrap(), "7KQ4M2XA");
        assert_eq!(normalize_code("7KQ4-M2XA").unwrap(), "7KQ4M2XA");
        assert_eq!(normalize_code("OIL4-M2XA").unwrap(), "0114M2XA");
        assert!(normalize_code("7KQ4M2X").is_none());
        assert!(normalize_code("7KQ4M2XU").is_none());
    }

    #[test]
    fn fingerprint_shape() {
        let fp = fingerprint(&[0u8; 32]);
        assert_eq!(fp.len(), 19);
        assert_eq!(fp.split(' ').count(), 4);
        assert_ne!(fp, fingerprint(&[1u8; 32]));
    }

    #[test]
    fn a_key_binding_is_only_valid_for_its_own_pair() {
        let a = Identity::from_seed(&[7u8; 32]);
        let b = Identity::from_seed(&[8u8; 32]);
        let sig = sign_binding(&a);
        assert!(verify_binding(&a.node_id(), &a.x25519_hex(), &sig));
        // Uppercase hex is the same pair.
        assert!(verify_binding(
            &a.node_id().to_ascii_uppercase(),
            &a.x25519_hex().to_ascii_uppercase(),
            &sig
        ));
        // A's signature says nothing about B's sealing key, or B's id.
        assert!(!verify_binding(&a.node_id(), &b.x25519_hex(), &sig));
        assert!(!verify_binding(&b.node_id(), &a.x25519_hex(), &sig));
        assert!(!verify_binding(&b.node_id(), &b.x25519_hex(), &sig));
        // Nor does anything malformed or absent.
        for bad in ["", "zz", &sig[..126], &sign_binding(&b)] {
            assert!(!verify_binding(&a.node_id(), &a.x25519_hex(), bad));
        }
        assert!(!verify_binding("zz", &a.x25519_hex(), &sig));
        assert!(!verify_binding(&a.node_id(), "zz", &sig));
    }

    #[test]
    fn a_signed_request_carries_its_own_binding() {
        let a = Identity::from_seed(&[7u8; 32]);
        let req = EnrollRequest::signed(&a, "laptop", "x86_64 linux");
        assert_eq!(req.node_id, a.node_id());
        assert_eq!(req.contract, crate::CONTRACT_VERSION);
        assert!(req.binding_ok());
        // A slot the hub rewrote to a sealing key it holds fails the proof.
        let b = Identity::from_seed(&[8u8; 32]);
        let mut forged = req.clone();
        forged.x25519_pub = b.x25519_hex();
        assert!(!forged.binding_ok());
        // As does one from before the proof existed.
        let mut unsigned = req.clone();
        unsigned.binding_sig = String::new();
        assert!(!unsigned.binding_ok());
        // The field is optional on the wire so it is refused by name.
        let v = serde_json::json!({
            "node_id": a.node_id(), "x25519_pub": a.x25519_hex(),
            "name": "laptop", "contract": crate::CONTRACT_VERSION,
        });
        let old: EnrollRequest = serde_json::from_value(v).unwrap();
        assert!(!old.binding_ok());
    }

    #[test]
    fn invite_round_trip() {
        let url = invite_url("https://hub.example/", "7KQ4M2XA");
        assert_eq!(url, "https://hub.example/#enroll=7KQ4M2XA");
        assert_eq!(
            parse_invite(&url).unwrap(),
            (
                Some("https://hub.example".to_string()),
                "7KQ4M2XA".to_string()
            )
        );
        assert_eq!(
            parse_invite("7kq4m2xa").unwrap(),
            (None, "7KQ4M2XA".to_string())
        );
        assert!(parse_invite("https://hub.example/#enroll=zz").is_none());
    }
}
