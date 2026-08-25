//! Signed policy bundles.
//!
//! The signing key never reaches the hub, so a compromised hub can serve stale
//! policy but not new policy. On the node, verification is what turns a file on
//! disk into rules the gate will act on: an unsigned or badly signed bundle
//! yields nothing, and nothing means every request is asked.

use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use super::Policy;

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("the bundle is not valid TOML: {0}")]
    Parse(String),
    #[error("the signature file is missing; a bundle the node cannot verify is not applied")]
    NoSignature,
    #[error("the signature does not match the bundle")]
    BadSignature,
    #[error("no signing key is configured on this node")]
    NoKey,
    #[error("the key is malformed: {0}")]
    BadKey(String),
}

/// Where the bundle, its signature, and the keys live.
pub struct Paths;

impl Paths {
    pub fn bundle() -> PathBuf {
        crate::config::Config::state_dir().join("policy.toml")
    }
    pub fn signature() -> PathBuf {
        crate::config::Config::state_dir().join("policy.toml.sig")
    }
    /// The public half, which the node needs and may safely hold.
    pub fn public_key() -> PathBuf {
        crate::config::Config::state_dir().join("policy-key.pub")
    }
    /// The private half. Kept off the hub, and only present on a machine that
    /// signs policy.
    pub fn signing_key() -> PathBuf {
        crate::config::Config::state_dir().join("policy-key")
    }
}

pub fn generate_key() -> (SigningKey, VerifyingKey) {
    let mut bytes = [0u8; 32];
    use rand::Rng;
    rand::rng().fill(&mut bytes);
    let signing = SigningKey::from_bytes(&bytes);
    let verifying = signing.verifying_key();
    (signing, verifying)
}

pub fn write_key(signing: &SigningKey) -> Result<(), BundleError> {
    let dir = Paths::signing_key();
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(Paths::signing_key(), hex::encode(signing.to_bytes()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(Paths::signing_key(), std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::write(
        Paths::public_key(),
        hex::encode(signing.verifying_key().to_bytes()),
    )?;
    Ok(())
}

fn read_hex_key(path: &Path) -> Result<[u8; 32], BundleError> {
    let text = std::fs::read_to_string(path)?;
    let bytes = hex::decode(text.trim()).map_err(|e| BundleError::BadKey(e.to_string()))?;
    bytes
        .try_into()
        .map_err(|_| BundleError::BadKey("expected 32 bytes".into()))
}

pub fn sign(bundle: &str) -> Result<String, BundleError> {
    let key = SigningKey::from_bytes(&read_hex_key(&Paths::signing_key())?);
    Ok(hex::encode(key.sign(bundle.as_bytes()).to_bytes()))
}

/// Load and verify. Every failure yields `Ok(None)` at the caller's level: the
/// node runs with no policy rather than refusing to start, and no policy means
/// everything is asked.
pub fn load() -> Result<Policy, BundleError> {
    let text = std::fs::read_to_string(Paths::bundle())?;
    let signature_hex =
        std::fs::read_to_string(Paths::signature()).map_err(|_| BundleError::NoSignature)?;
    let key_bytes = read_hex_key(&Paths::public_key()).map_err(|e| match e {
        BundleError::Io(_) => BundleError::NoKey,
        other => other,
    })?;
    verify(&text, signature_hex.trim(), &key_bytes)?;
    toml::from_str(&text).map_err(|e| BundleError::Parse(e.to_string()))
}

pub fn verify(bundle: &str, signature_hex: &str, public_key: &[u8; 32]) -> Result<(), BundleError> {
    let key =
        VerifyingKey::from_bytes(public_key).map_err(|e| BundleError::BadKey(e.to_string()))?;
    let raw = hex::decode(signature_hex).map_err(|_| BundleError::BadSignature)?;
    let bytes: [u8; 64] = raw.try_into().map_err(|_| BundleError::BadSignature)?;
    key.verify(bundle.as_bytes(), &Signature::from_bytes(&bytes))
        .map_err(|_| BundleError::BadSignature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Request, Verdict, WORKING_AGREEMENTS};

    #[test]
    fn a_signed_bundle_verifies() {
        let (signing, verifying) = generate_key();
        let sig = hex::encode(signing.sign(WORKING_AGREEMENTS.as_bytes()).to_bytes());
        assert!(verify(WORKING_AGREEMENTS, &sig, &verifying.to_bytes()).is_ok());
    }

    #[test]
    fn a_tampered_bundle_does_not_verify() {
        let (signing, verifying) = generate_key();
        let sig = hex::encode(signing.sign(WORKING_AGREEMENTS.as_bytes()).to_bytes());
        // One rule flipped from deny to allow: exactly the edit signing exists
        // to catch.
        let tampered = WORKING_AGREEMENTS.replacen(
            "id = \"no-merge\"\nverdict = \"deny\"",
            "id = \"no-merge\"\nverdict = \"allow\"",
            1,
        );
        assert_ne!(
            tampered, WORKING_AGREEMENTS,
            "the test must actually change it"
        );
        assert!(matches!(
            verify(&tampered, &sig, &verifying.to_bytes()),
            Err(BundleError::BadSignature)
        ));
    }

    #[test]
    fn another_key_does_not_verify() {
        let (signing, _) = generate_key();
        let (_, other) = generate_key();
        let sig = hex::encode(signing.sign(WORKING_AGREEMENTS.as_bytes()).to_bytes());
        assert!(verify(WORKING_AGREEMENTS, &sig, &other.to_bytes()).is_err());
    }

    #[test]
    fn a_malformed_signature_is_refused_not_ignored() {
        let (_, verifying) = generate_key();
        for sig in ["", "nothex", "aa"] {
            assert!(
                verify(WORKING_AGREEMENTS, sig, &verifying.to_bytes()).is_err(),
                "{sig}"
            );
        }
    }

    #[test]
    fn a_verified_bundle_still_enforces_its_rules() {
        // Signing is only worth having if what it protects is the thing that
        // decides.
        let p: Policy = toml::from_str(WORKING_AGREEMENTS).unwrap();
        let d = p.decide(&Request {
            channel: "work",
            kind: Some("execute"),
            title: "gh pr merge 12",
            command: Some("gh pr merge 12"),
        });
        assert_eq!(d.verdict, Verdict::Deny);
    }
}
