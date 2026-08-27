//! Enrollment: the one public write on the hub. An enrolled node opens a slot
//! under a short-lived code; the new node fills it with its public keys and a
//! name; the enrolled node fetches, the operator compares fingerprints, and the
//! new node is admitted. Nothing in the slot is secret, so the hub stores it in
//! the clear; the fingerprint comparison is what defeats a hub that substitutes
//! keys.

use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
