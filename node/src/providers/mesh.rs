//! What provider credentials need from the mesh: a claim on the right to
//! refresh one, who else should hold a sign-in, and a way to hand it to them.
//! Behind a trait so the providers can be driven without a hub.

use sha2::{Digest, Sha256};

use crate::broker::Credential;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimResult {
    /// This node refreshes this version.
    Won,
    /// Another holder has the claim on this version now; its renewed copy
    /// arrives by handoff. The holder's node id, when the hub named it.
    Held(Option<String>),
    /// The hub knows this version was claimed past or renewed already: this
    /// copy is stale, and spending its refresh token would be refused. The
    /// node that renewed it, when the hub was told.
    Stale(Option<String>),
    /// The hub predates claims.
    Unsupported,
    /// The hub could not be asked.
    Unreachable(String),
}

impl std::fmt::Display for ClaimResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Won => f.write_str("won"),
            Self::Held(Some(holder)) => write!(f, "held by {holder}"),
            Self::Held(None) => f.write_str("held by another holder"),
            Self::Stale(Some(by)) => write!(f, "stale, renewed by {by}"),
            Self::Stale(None) => f.write_str("stale"),
            Self::Unsupported => f.write_str("unsupported by the hub"),
            Self::Unreachable(error) => write!(f, "unreachable ({error})"),
        }
    }
}

/// What a node says it does about renewing the shared sign-ins it holds,
/// carried on each sign-in it publishes (`renews`), so every holder works
/// out the same renewer ([`designated_renewer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewRole {
    /// `[mesh] renew_credentials` is set: it renews first.
    Configured,
    /// It runs where it is always on (Kubernetes), which makes it the
    /// renewer when it is the only such holder and none is configured.
    AlwaysOn,
}

impl RenewRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::AlwaysOn => "always_on",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "configured" => Some(Self::Configured),
            "always_on" => Some(Self::AlwaysOn),
            _ => None,
        }
    }
}

/// Which of `holders` renews a shared sign-in first: the first one set to
/// (`[mesh] renew_credentials`), or else the only one always on. `None` when
/// neither picks one out, and every holder is a fallback.
pub fn designated_renewer<'a>(
    holders: impl IntoIterator<Item = (&'a str, Option<RenewRole>)>,
) -> Option<&'a str> {
    let holders: Vec<_> = holders.into_iter().collect();
    if let Some((node, _)) = holders
        .iter()
        .find(|(_, role)| *role == Some(RenewRole::Configured))
    {
        return Some(node);
    }
    let mut always_on = holders
        .iter()
        .filter(|(_, role)| *role == Some(RenewRole::AlwaysOn));
    match (always_on.next(), always_on.next()) {
        (Some((node, _)), None) => Some(node),
        _ => None,
    }
}

#[async_trait::async_trait]
pub trait CredentialMesh: Send + Sync {
    /// Claim the refresh of the sign-in `key` names, at `version`.
    async fn claim(&self, key: &str, version: u64) -> ClaimResult;
    /// Tell the hub this node renewed `version` of the sign-in `key` names,
    /// after winning its claim, so a holder still on it is told it is stale
    /// and who has the newer copy.
    async fn renewed(&self, key: &str, version: u64) -> Result<(), String>;
    /// Other members, able to receive a sealed handoff, bound to any of
    /// `channels`.
    fn members_in(&self, channels: &[String]) -> Vec<String>;
    /// Hand `credential` to each of `to`.
    fn hand_off(&self, name: &str, credential: &Credential, to: &[String]);
    /// Ask each of `from` for its copy of `credential`, held here as `name`,
    /// should it hold a newer one; it answers with a handoff.
    fn request_copy(&self, name: &str, credential: &Credential, from: &[String]);
    /// What `node` says about renewing the shared sign-ins it holds, as its
    /// published providers carry it. `None` for none, or not known.
    fn renew_role(&self, node: &str) -> Option<RenewRole>;
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
    fn the_renewer_is_the_configured_holder_or_else_the_only_one_always_on() {
        use RenewRole::*;
        assert_eq!(
            designated_renewer([("a", None), ("b", Some(AlwaysOn)), ("c", Some(Configured))]),
            Some("c")
        );
        assert_eq!(
            designated_renewer([("a", None), ("b", Some(AlwaysOn))]),
            Some("b")
        );
        assert_eq!(
            designated_renewer([("a", Some(AlwaysOn)), ("b", Some(AlwaysOn))]),
            None
        );
        assert_eq!(designated_renewer([("a", None), ("b", None)]), None);
        assert_eq!(
            designated_renewer([("a", Some(Configured)), ("b", Some(Configured))]),
            Some("a")
        );
    }

    #[test]
    fn a_claim_key_is_opaque_and_stable() {
        let key = claim_key("0190a1b2-0000-7000-8000-000000000000");
        assert_eq!(key.len(), 64);
        assert_eq!(key, claim_key("0190a1b2-0000-7000-8000-000000000000"));
        assert_ne!(key, claim_key("0190a1b2-0000-7000-8000-000000000001"));
        assert!(!key.contains("0190"));
    }
}
