//! Admin statements: an operator's confirmation on one node, carried to a
//! peer.
//!
//! ```text
//! { issuer, target, op_hash, summary, asserted_at_ms, not_after_ms, session, nonce, sig }
//! ```
//!
//! An admin action on a peer (its configuration, a provider sign-in on it)
//! is confirmed where the operator is: on node A, with A's passkey, for
//! exactly that operation on exactly that peer. A then signs a statement with
//! its node key saying so, and sends it with the command. The peer B never
//! sees the passkey; what it checks is that A is a node it lets administer it,
//! that A's key signed the statement, that the statement names B and the very
//! operation B is about to run ([`op_hash`]), that it is fresh, and that it
//! has not been spent before.
//!
//! - `issuer`, `target`: node ids (Ed25519 public keys, hex).
//! - `op_hash`: [`op_hash`] of the command as sent, with the statement and the
//!   frame deadline left out.
//! - `summary`: the line the operator confirmed, as A's prompt showed it.
//! - `asserted_at_ms`, `not_after_ms`: when A confirmed it, and when the
//!   statement lapses; at most [`STATEMENT_TTL_MS`] apart.
//! - `session`: which of A's operator sessions confirmed it (a prefix of the
//!   session's hash), or `local` for the operator at A's own machine.
//! - `nonce`: 16 random bytes, hex; B spends each once.
//! - `sig`: Ed25519 by `issuer` over [`AdminStatement::signing_bytes`].

use ed25519_dalek::{Signature, VerifyingKey};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::keys::{key32, Identity};
use crate::{put_bytes, put_str};

/// How long a statement may be spent after its operator confirmed it: the
/// same two minutes a passkey challenge lives.
pub const STATEMENT_TTL_MS: i64 = 2 * 60 * 1000;

/// The longest summary a statement carries. A longer one is not cut, which
/// would sign less than the operator read: it does not verify.
pub const MAX_SUMMARY: usize = 1024;

/// A command's fields a statement does not cover: the statement itself, and
/// the deadline the frame carries, which the statement's own `not_after_ms`
/// bounds.
const UNBOUND: &[&str] = &["statement", "not_after_ms"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminStatement {
    pub issuer: String,
    pub target: String,
    pub op_hash: String,
    pub summary: String,
    pub asserted_at_ms: i64,
    pub not_after_ms: i64,
    pub session: String,
    pub nonce: String,
    pub sig: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StatementError {
    #[error("malformed admin statement field: {0}")]
    Malformed(&'static str),
    #[error("the admin statement's signature does not verify")]
    BadSignature,
}

/// The operation a statement is for: `command` (a command as it serializes,
/// its `op` included) on `target`, with [`UNBOUND`] fields left out of the
/// command's top level. Objects are hashed with their keys sorted, so the
/// hash does not depend on the order a build writes fields in; any field a
/// receiver's build does not read changes what it hashes, and the statement
/// is refused rather than applied to less than the operator confirmed.
pub fn op_hash(target: &str, command: &Value) -> String {
    let mut bound = command.clone();
    if let Some(map) = bound.as_object_mut() {
        for key in UNBOUND {
            map.remove(*key);
        }
    }
    let mut canonical = String::new();
    write_canonical(&bound, &mut canonical);
    let mut out = crate::version_label("admin-op").into_bytes();
    put_str(&mut out, target);
    put_bytes(&mut out, canonical.as_bytes());
    hex::encode(Sha256::digest(&out))
}

/// JSON with every object's keys in byte order and no whitespace.
fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

impl AdminStatement {
    /// Sign a statement as `issuer` for `op_hash` on `target`, confirmed at
    /// `asserted_at_ms` and spendable for [`STATEMENT_TTL_MS`].
    pub fn sign(
        issuer: &Identity,
        target: &str,
        op_hash: &str,
        summary: &str,
        session: &str,
        asserted_at_ms: i64,
    ) -> Self {
        let mut nonce = [0u8; 16];
        OsRng.fill_bytes(&mut nonce);
        Self::sign_with_nonce(
            issuer,
            target,
            op_hash,
            summary,
            session,
            asserted_at_ms,
            &hex::encode(nonce),
        )
    }

    /// As [`sign`](Self::sign), with the nonce given: for test vectors.
    #[doc(hidden)]
    pub fn sign_with_nonce(
        issuer: &Identity,
        target: &str,
        op_hash: &str,
        summary: &str,
        session: &str,
        asserted_at_ms: i64,
        nonce: &str,
    ) -> Self {
        let mut statement = AdminStatement {
            issuer: issuer.node_id(),
            target: target.to_string(),
            op_hash: op_hash.to_string(),
            summary: summary.to_string(),
            asserted_at_ms,
            not_after_ms: asserted_at_ms.saturating_add(STATEMENT_TTL_MS),
            session: session.to_string(),
            nonce: nonce.to_string(),
            sig: String::new(),
        };
        statement.sig = hex::encode(issuer.sign(&statement.signing_bytes()).to_bytes());
        statement
    }

    /// What the issuer signs: every field but the signature, each
    /// length-prefixed, under this contract's domain label.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = crate::version_label("admin-statement").into_bytes();
        put_str(&mut out, &self.issuer);
        put_str(&mut out, &self.target);
        put_str(&mut out, &self.op_hash);
        put_str(&mut out, &self.summary);
        out.extend_from_slice(&self.asserted_at_ms.to_be_bytes());
        out.extend_from_slice(&self.not_after_ms.to_be_bytes());
        put_str(&mut out, &self.session);
        put_str(&mut out, &self.nonce);
        out
    }

    /// Whether `issuer`'s key signed this statement as it stands, and its
    /// fields have the shapes they must. Says nothing about whether the
    /// statement is for an operation, a node or a moment: the receiver
    /// checks those against what it is about to run.
    pub fn verify(&self) -> Result<(), StatementError> {
        let issuer = key32(&self.issuer).ok_or(StatementError::Malformed("issuer"))?;
        if key32(&self.target).is_none() {
            return Err(StatementError::Malformed("target"));
        }
        if key32(&self.op_hash).is_none() {
            return Err(StatementError::Malformed("op_hash"));
        }
        if self.summary.chars().count() > MAX_SUMMARY {
            return Err(StatementError::Malformed("summary"));
        }
        if hex::decode(&self.nonce).map_or(true, |n| n.len() != 16) {
            return Err(StatementError::Malformed("nonce"));
        }
        let sig: [u8; 64] = hex::decode(&self.sig)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or(StatementError::Malformed("sig"))?;
        let vk =
            VerifyingKey::from_bytes(&issuer).map_err(|_| StatementError::Malformed("issuer"))?;
        if !crate::keys::verify(&vk, &self.signing_bytes(), &Signature::from_bytes(&sig)) {
            return Err(StatementError::BadSignature);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ids() -> (Identity, Identity) {
        (
            Identity::from_seed(&[41u8; 32]),
            Identity::from_seed(&[42u8; 32]),
        )
    }

    #[test]
    fn the_op_hash_ignores_key_order_and_the_unbound_fields_only() {
        let (_, b) = ids();
        let target = b.node_id();
        let one = json!({ "op": "config_patch", "expected_hash": "h", "ops": [{ "op": "set", "key": "a", "value": 1 }] });
        let reordered = json!({ "ops": [{ "value": 1, "key": "a", "op": "set" }], "expected_hash": "h", "op": "config_patch" });
        let with_unbound = json!({
            "op": "config_patch", "expected_hash": "h",
            "ops": [{ "op": "set", "key": "a", "value": 1 }],
            "statement": { "anything": true }, "not_after_ms": 5,
        });
        let h = op_hash(&target, &one);
        assert_eq!(op_hash(&target, &reordered), h);
        assert_eq!(op_hash(&target, &with_unbound), h);
        // Another value, another key, another expected version, another
        // target, another op: each a different operation.
        for other in [
            json!({ "op": "config_patch", "expected_hash": "h", "ops": [{ "op": "set", "key": "a", "value": 2 }] }),
            json!({ "op": "config_patch", "expected_hash": "h", "ops": [{ "op": "set", "key": "b", "value": 1 }] }),
            json!({ "op": "config_patch", "expected_hash": "g", "ops": [{ "op": "set", "key": "a", "value": 1 }] }),
            json!({ "op": "config_read", "expected_hash": "h", "ops": [{ "op": "set", "key": "a", "value": 1 }] }),
            json!({ "op": "config_patch", "expected_hash": "h", "ops": [{ "op": "set", "key": "a", "value": 1, "statement": 1 }] }),
        ] {
            assert_ne!(op_hash(&target, &other), h, "{other}");
        }
        assert_ne!(op_hash(&ids().0.node_id(), &one), h);
        // An `UNBOUND` name is only left out at the command's top level.
        assert_ne!(
            op_hash(&target, &json!({ "op": "x", "inner": { "statement": 1 } })),
            op_hash(&target, &json!({ "op": "x", "inner": {} }))
        );
    }

    #[test]
    fn a_statement_verifies_and_any_change_to_it_does_not() {
        let (a, b) = ids();
        let s = AdminStatement::sign(
            &a,
            &b.node_id(),
            &"ab".repeat(32),
            "On beta: x",
            "local",
            1_000,
        );
        s.verify().unwrap();
        assert_eq!(s.not_after_ms, 1_000 + STATEMENT_TTL_MS);
        // Every field is signed.
        type Mutation = Box<dyn Fn(&mut AdminStatement)>;
        let mutations: Vec<Mutation> = vec![
            Box::new(|s| s.target = ids().0.node_id()),
            Box::new(|s| s.op_hash = "cd".repeat(32)),
            Box::new(|s| s.summary.push('!')),
            Box::new(|s| s.asserted_at_ms += 1),
            Box::new(|s| s.not_after_ms += 1),
            Box::new(|s| s.session = "0123456789ab".into()),
            Box::new(|s| s.nonce = "00".repeat(16)),
        ];
        for mutate in mutations {
            let mut t = s.clone();
            mutate(&mut t);
            assert_eq!(t.verify(), Err(StatementError::BadSignature), "{t:?}");
        }
        // Signed by another node: it does not verify as the issuer's.
        let mut t = AdminStatement::sign(&b, &b.node_id(), &s.op_hash, &s.summary, "local", 1_000);
        t.issuer = a.node_id();
        assert_eq!(t.verify(), Err(StatementError::BadSignature));
        // Two statements for one operation still differ by their nonce.
        let again = AdminStatement::sign(&a, &b.node_id(), &s.op_hash, &s.summary, "local", 1_000);
        assert_ne!(again.nonce, s.nonce);
        let mut t = s.clone();
        t.nonce = "zz".into();
        assert_eq!(t.verify(), Err(StatementError::Malformed("nonce")));
    }

    #[derive(serde::Deserialize)]
    struct VectorFile {
        contract_version: u32,
        issuer_seed_hex: String,
        target_seed_hex: String,
        command: Value,
        op_hash_hex: String,
        summary: String,
        session: String,
        asserted_at_ms: i64,
        nonce_hex: String,
        signing_bytes_hex: String,
        sig_hex: String,
    }
    const VECTORS: &str = include_str!("../../spec/vectors/admin-statement.json");

    /// The op hash, the signed bytes and the signature (Ed25519 is
    /// deterministic) are pinned for a fixed statement.
    #[test]
    fn matches_spec_vectors() {
        let f: VectorFile = serde_json::from_str(VECTORS).unwrap();
        assert_eq!(f.contract_version, crate::CONTRACT_VERSION);
        let issuer = Identity::from_seed(&key32(&f.issuer_seed_hex).unwrap());
        let target = Identity::from_seed(&key32(&f.target_seed_hex).unwrap());
        let hash = op_hash(&target.node_id(), &f.command);
        assert_eq!(hash, f.op_hash_hex);
        let s = AdminStatement::sign_with_nonce(
            &issuer,
            &target.node_id(),
            &hash,
            &f.summary,
            &f.session,
            f.asserted_at_ms,
            &f.nonce_hex,
        );
        assert_eq!(hex::encode(s.signing_bytes()), f.signing_bytes_hex);
        assert_eq!(s.sig, f.sig_hex);
        s.verify().unwrap();
    }
}
