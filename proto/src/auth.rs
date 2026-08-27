//! Signed hub requests. Stateless: every request carries the node's public key, a
//! timestamp, and an Ed25519 signature over a canonical descriptor of method,
//! path (with query), body hash, and timestamp. The hub checks freshness and,
//! for state-changing methods, remembers the signature so an identical replay is
//! refused (Ed25519 is deterministic, so a byte-for-byte replay carries the same
//! signature — the signature is the nonce).

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::keys::Identity;
use crate::put_str;

pub const HEADER_PUBLIC_KEY: &str = "tracon-public-key";
pub const HEADER_TIMESTAMP: &str = "tracon-timestamp";
pub const HEADER_SIGNATURE: &str = "tracon-signature";

pub struct AuthRequest {
    method: String,
    path: String,
    body_sha256: [u8; 32],
    timestamp: u64,
}

impl AuthRequest {
    /// `path` includes the query string. `timestamp` is Unix seconds.
    pub fn new(method: &str, path: &str, body: &[u8], timestamp: u64) -> Self {
        Self {
            method: method.to_string(),
            path: path.to_string(),
            body_sha256: Sha256::digest(body).into(),
            timestamp,
        }
    }

    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(crate::version_label("relay-auth").as_bytes());
        put_str(&mut out, &self.method);
        put_str(&mut out, &self.path);
        out.extend_from_slice(&self.body_sha256);
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out
    }
}

pub fn sign_request(identity: &Identity, request: &AuthRequest) -> [u8; 64] {
    identity.sign(&request.signing_bytes()).to_bytes()
}

pub fn verify_request(public_key: &[u8; 32], signature: &[u8; 64], request: &AuthRequest) -> bool {
    let Ok(pk) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    crate::keys::verify(
        &pk,
        &request.signing_bytes(),
        &Signature::from_bytes(signature),
    )
}

/// The three headers a signed request carries, as `(name, value)` pairs.
pub fn signed_headers(
    identity: &Identity,
    method: &str,
    path: &str,
    body: &[u8],
    timestamp: u64,
) -> [(&'static str, String); 3] {
    let req = AuthRequest::new(method, path, body, timestamp);
    [
        (HEADER_PUBLIC_KEY, identity.node_id()),
        (HEADER_TIMESTAMP, timestamp.to_string()),
        (HEADER_SIGNATURE, hex::encode(sign_request(identity, &req))),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::key32;

    fn req() -> AuthRequest {
        AuthRequest::new("POST", "/v0/frames?channel=personal", b"{}", 1_787_000_000)
    }

    #[test]
    fn round_trip_and_tamper() {
        let id = Identity::from_seed(&[4u8; 32]);
        let pk = id.verifying_key().to_bytes();
        let sig = sign_request(&id, &req());
        assert!(verify_request(&pk, &sig, &req()));
        for t in [
            AuthRequest::new("GET", "/v0/frames?channel=personal", b"{}", 1_787_000_000),
            AuthRequest::new("POST", "/v0/frames?channel=work", b"{}", 1_787_000_000),
            AuthRequest::new("POST", "/v0/frames?channel=personal", b"{ }", 1_787_000_000),
            AuthRequest::new("POST", "/v0/frames?channel=personal", b"{}", 1_787_000_001),
        ] {
            assert!(!verify_request(&pk, &sig, &t));
        }
        assert!(!verify_request(&[0xff; 32], &sig, &req()));
        let other = Identity::from_seed(&[5u8; 32]);
        assert!(!verify_request(
            &other.verifying_key().to_bytes(),
            &sig,
            &req()
        ));
    }

    #[derive(serde::Deserialize)]
    struct VectorFile {
        contract_version: u32,
        requests: Vec<V>,
    }
    #[derive(serde::Deserialize)]
    struct V {
        method: String,
        path: String,
        body_hex: String,
        timestamp: u64,
        signer_seed_hex: String,
        canon_hex: String,
        public_key_hex: String,
        signature_hex: String,
    }
    const VECTORS: &str = include_str!("../../spec/vectors/auth.json");

    #[test]
    fn matches_spec_vectors() {
        let f: VectorFile = serde_json::from_str(VECTORS).unwrap();
        assert_eq!(f.contract_version, crate::CONTRACT_VERSION);
        for v in &f.requests {
            let r = AuthRequest::new(
                &v.method,
                &v.path,
                &hex::decode(&v.body_hex).unwrap(),
                v.timestamp,
            );
            assert_eq!(hex::encode(r.signing_bytes()), v.canon_hex);
            let s = Identity::from_seed(&key32(&v.signer_seed_hex).unwrap());
            assert_eq!(s.node_id(), v.public_key_hex);
            let sig = sign_request(&s, &r);
            assert_eq!(hex::encode(sig), v.signature_hex);
            assert!(verify_request(&key32(&v.public_key_hex).unwrap(), &sig, &r));
        }
    }
}
