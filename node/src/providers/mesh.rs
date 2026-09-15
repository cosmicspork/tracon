//! What provider credentials need from the mesh: a claim on the right to
//! refresh one, who else should hold a sign-in, and a way to hand it to them.
//! Behind a trait so the providers can be driven without a hub.

use sha2::{Digest, Sha256};

use crate::broker::Credential;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimResult {
    /// This node refreshes this version.
    Won,
    /// Another holder has it, or has already moved past it.
    Lost,
    /// The hub predates claims.
    Unsupported,
    /// The hub could not be asked.
    Unreachable(String),
}

#[async_trait::async_trait]
pub trait CredentialMesh: Send + Sync {
    /// Claim the refresh of the sign-in `key` names, at `version`.
    async fn claim(&self, key: &str, version: u64) -> ClaimResult;
    /// Other members, able to receive a sealed handoff, bound to any of
    /// `channels`.
    fn members_in(&self, channels: &[String]) -> Vec<String>;
    /// Hand `credential` to each of `to`.
    fn hand_off(&self, name: &str, credential: &Credential, to: &[String]);
}

/// The hub's key for one sign-in's refreshes. A hash, so the hub learns
/// neither the credential's name nor which provider it is for.
pub fn claim_key(grant_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"tracon/credential-refresh\0");
    hash.update(grant_id.as_bytes());
    hex::encode(hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_key_is_opaque_and_stable() {
        let key = claim_key("0190a1b2-0000-7000-8000-000000000000");
        assert_eq!(key.len(), 64);
        assert_eq!(key, claim_key("0190a1b2-0000-7000-8000-000000000000"));
        assert_ne!(key, claim_key("0190a1b2-0000-7000-8000-000000000001"));
        assert!(!key.contains("0190"));
    }
}
