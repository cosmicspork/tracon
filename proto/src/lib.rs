//! tracon mesh wire contract. One crate, depended on by the node and the hub, so
//! every byte that crosses a machine boundary is defined in exactly one place.
//!
//! The shapes are borrowed from an earlier end-to-end-encrypted project's trust
//! contract (identity derivation,
//! ECIES sealed boxes, seal-then-sign envelopes, signed relay requests, key
//! epochs) and re-labelled; the crate is vendored rather than depended on so
//! tracon's wire contract can move on its own cadence. `spec/README.md` is the
//! prose description and `spec/vectors/*.json` pin the bytes; every module has a
//! `matches_spec_vectors` test.

/// The wire contract version, reported by the hub at `GET /v0/info`. Advances
/// additively; moving it does not rotate any key. See [`CONTRACT_MAJOR`].
pub const CONTRACT_VERSION: u32 = 2;

/// The cryptographic era embedded in every HKDF and signing label. Bumps only on
/// a key-rotating break. Holding it fixed across [`CONTRACT_VERSION`] bumps is
/// what keeps every enrolled identity and handed-off keyring valid.
pub(crate) const CONTRACT_MAJOR: u32 = 0;

/// Domain label for one contract operation, tagged with [`CONTRACT_MAJOR`].
pub(crate) fn version_label(operation: &str) -> String {
    format!("tracon/v{CONTRACT_MAJOR}/{operation}")
}

/// The cryptographic era, exposed for `/v0/info` and diagnostics.
pub fn contract_major() -> u32 {
    CONTRACT_MAJOR
}

pub mod auth;
pub mod enroll;
pub mod envelope;
pub mod frame;
pub mod keyring;
pub mod keys;

/// Length-prefixed bytes: u32 big-endian length then the bytes. Shared by every
/// canonical encoding in this crate.
pub(crate) fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

pub(crate) fn put_str(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}
