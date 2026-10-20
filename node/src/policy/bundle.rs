//! Signed policy bundles.
//!
//! The signing key never reaches the hub, so a compromised hub can serve stale
//! policy but not new policy. On the node, verification is what turns a file on
//! disk into rules the gate will act on: an unsigned or badly signed bundle
//! yields nothing, and nothing means every request is asked.

use std::io::Write;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

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
    #[error("the bundle is signed by a key this node does not trust")]
    KeyMismatch,
    #[error("no policy key is installed here, and this bundle did not arrive through enrollment")]
    Untrusted,
    #[error("a policy bundle or signing identity already exists; initialization never replaces custom policy or rotates keys")]
    AlreadyConfigured,
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
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(Paths::signing_key())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(hex::encode(signing.to_bytes()).as_bytes())?;
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

/// The configured trust identity, never the signing secret.
pub fn public_identity() -> Result<String, BundleError> {
    Ok(hex::encode(read_hex_key(&Paths::public_key())?))
}

/// A verified immutable bundle. The signing key is deliberately not part of
/// this shape: callers can preview and distribute a signature, never retrieve
/// the key that made it.
#[derive(Debug, Clone)]
pub struct SignedBundle {
    pub toml: String,
    pub sig_hex: String,
    pub pubkey_hex: String,
    pub sha256: String,
    pub policy: Policy,
}

pub fn sha256(bundle: &str) -> String {
    hex::encode(Sha256::digest(bundle.as_bytes()))
}

/// Parse and sign an in-memory policy with this node's already configured key.
/// It neither creates nor replaces a key or a bundle.
pub fn preview(bundle: &str) -> Result<SignedBundle, BundleError> {
    let mut policy: Policy =
        toml::from_str(bundle).map_err(|e| BundleError::Parse(e.to_string()))?;
    let public = read_hex_key(&Paths::public_key()).map_err(|e| match e {
        BundleError::Io(_) => BundleError::NoKey,
        other => other,
    })?;
    let signing =
        SigningKey::from_bytes(&read_hex_key(&Paths::signing_key()).map_err(|e| match e {
            BundleError::Io(_) => BundleError::NoKey,
            other => other,
        })?);
    if signing.verifying_key().to_bytes() != public {
        return Err(BundleError::KeyMismatch);
    }
    policy.trusted = true;
    Ok(SignedBundle {
        toml: bundle.to_string(),
        sig_hex: hex::encode(signing.sign(bundle.as_bytes()).to_bytes()),
        pubkey_hex: hex::encode(public),
        sha256: sha256(bundle),
        policy,
    })
}

/// Create the initial local signing identity and shipped bundle, once.
///
/// This is deliberately an explicit, local operator action. Any pre-existing
/// policy artifact — including a partial prior setup — is a refusal rather than
/// a reason to replace an operator's custom policy or rotate its trust key.
pub fn initialize_shipped() -> Result<SignedBundle, BundleError> {
    let paths = [
        Paths::bundle(),
        Paths::signature(),
        Paths::public_key(),
        Paths::signing_key(),
    ];
    if paths.iter().any(|path| path.exists()) {
        return Err(BundleError::AlreadyConfigured);
    }
    let state_dir = crate::config::Config::state_dir();
    std::fs::create_dir_all(&state_dir)?;
    let initialize_lock = state_dir.join(".policy-initialize.lock");
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&initialize_lock)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                BundleError::AlreadyConfigured
            } else {
                BundleError::Io(error)
            }
        })?;
    drop(lock);
    // Recheck after the cross-process reservation. A manually created artifact
    // wins over setup; keeping the reservation means a later request cannot
    // turn that partial situation into an overwrite.
    if paths.iter().any(|path| path.exists()) {
        return Err(BundleError::AlreadyConfigured);
    }

    let (signing, _) = generate_key();
    write_key(&signing)?;
    let signed = preview(super::WORKING_AGREEMENTS)?;
    let write_atomic = |path: PathBuf, text: &str| -> Result<(), BundleError> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    };
    write_atomic(Paths::signature(), &signed.sig_hex)?;
    write_atomic(Paths::bundle(), &signed.toml)?;
    std::fs::remove_file(initialize_lock)?;
    Ok(signed)
}

/// Read the installed signed bundle as one immutable unit. A caller that wants
/// to replace it must still call [`install`], which verifies before writing.
pub fn current() -> Result<SignedBundle, BundleError> {
    let toml = std::fs::read_to_string(Paths::bundle())?;
    let sig_hex =
        std::fs::read_to_string(Paths::signature()).map_err(|_| BundleError::NoSignature)?;
    let public = read_hex_key(&Paths::public_key()).map_err(|e| match e {
        BundleError::Io(_) => BundleError::NoKey,
        other => other,
    })?;
    verify(&toml, sig_hex.trim(), &public)?;
    let mut policy: Policy =
        toml::from_str(&toml).map_err(|e| BundleError::Parse(e.to_string()))?;
    policy.trusted = true;
    Ok(SignedBundle {
        sha256: sha256(&toml),
        toml,
        sig_hex: sig_hex.trim().to_string(),
        pubkey_hex: hex::encode(public),
        policy,
    })
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
    let mut policy: Policy =
        toml::from_str(&text).map_err(|e| BundleError::Parse(e.to_string()))?;
    policy.trusted = true;
    Ok(policy)
}

/// Install a bundle that arrived over the mesh. The public key is trusted only
/// if it is the one already installed, or — when none is installed —
/// `trust_new_key` says this is the enrollment handoff, the one moment the
/// operator has just compared fingerprints. Verified and parsed before any
/// file is touched; written through temp files so a crash cannot leave a
/// bundle without its signature.
pub fn install(
    bundle: &str,
    signature_hex: &str,
    public_key_hex: &str,
    trust_new_key: bool,
) -> Result<Policy, BundleError> {
    let offered: [u8; 32] = hex::decode(public_key_hex.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| BundleError::BadKey("expected 32 hex bytes".into()))?;
    match read_hex_key(&Paths::public_key()) {
        Ok(installed) if installed != offered => return Err(BundleError::KeyMismatch),
        Ok(_) => {}
        Err(BundleError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            if !trust_new_key {
                return Err(BundleError::Untrusted);
            }
        }
        Err(e) => return Err(e),
    }
    verify(bundle, signature_hex.trim(), &offered)?;
    let mut policy: Policy =
        toml::from_str(bundle).map_err(|e| BundleError::Parse(e.to_string()))?;
    policy.trusted = true;

    let dir = Paths::bundle();
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let write_atomic = |path: PathBuf, text: &str| -> Result<(), BundleError> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    };
    write_atomic(Paths::public_key(), &hex::encode(offered))?;
    write_atomic(Paths::signature(), signature_hex.trim())?;
    write_atomic(Paths::bundle(), bundle)?;
    Ok(policy)
}

/// What a bundle offered over the mesh is to the one installed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    /// Newer than what is installed, or nothing is: [`install`] decides.
    Install,
    /// Exactly the installed bundle. Redelivery writes nothing and is
    /// answered as applied.
    AlreadyInstalled,
    /// Refused, with why: a mesh redelivery never moves this node back to an
    /// older bundle, nor swaps one version's bundle for another.
    Refuse(String),
}

/// Judge an offered bundle, signed by `key`, against `installed` (its
/// version, hash and key). The key is checked first, so a bundle from a key
/// this node does not trust is refused for that, whatever its version says.
pub fn judge_offer(
    installed: Option<(u32, &str, &str)>,
    version: u32,
    sha256: &str,
    key: &str,
) -> Offer {
    match installed {
        None => Offer::Install,
        Some((_, _, trusted)) if !trusted.trim().eq_ignore_ascii_case(key.trim()) => {
            Offer::Refuse(BundleError::KeyMismatch.to_string())
        }
        Some((current, _, _)) if version < current => {
            Offer::Refuse(format!("older than installed v{current}"))
        }
        Some((current, hash, _)) if version == current && hash == sha256 => Offer::AlreadyInstalled,
        Some((current, _, _)) if version == current => {
            Offer::Refuse(format!("conflicting bundle for v{current}"))
        }
        Some(_) => Offer::Install,
    }
}

/// [`judge_offer`] against the verified bundle on disk. Only the mesh receive
/// paths ask: an operator applying or signing locally is acting directly.
/// A bundle that does not parse, or a node whose installed files do not
/// verify, is left to [`install`], as before this check existed.
pub fn offer(bundle: &str, public_key_hex: &str) -> Offer {
    let Ok(offered) = toml::from_str::<Policy>(bundle) else {
        return Offer::Install;
    };
    let installed = current().ok();
    judge_offer(
        installed
            .as_ref()
            .map(|b| (b.policy.version, b.sha256.as_str(), b.pubkey_hex.as_str())),
        offered.version,
        &sha256(bundle),
        public_key_hex,
    )
}

/// The bundle, signature, and public key as this node holds them, for handing
/// to a peer. `None` when this node has no bundle.
pub fn export() -> Option<(String, String, String)> {
    let toml = std::fs::read_to_string(Paths::bundle()).ok()?;
    let sig = std::fs::read_to_string(Paths::signature()).ok()?;
    let key = std::fs::read_to_string(Paths::public_key()).ok()?;
    Some((toml, sig.trim().to_string(), key.trim().to_string()))
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
    fn an_older_offer_is_refused_and_names_the_installed_version() {
        assert_eq!(
            judge_offer(Some((21, "new", "k")), 18, "old", "k"),
            Offer::Refuse("older than installed v21".into())
        );
    }

    #[test]
    fn the_installed_bundle_offered_again_is_already_installed() {
        assert_eq!(
            judge_offer(Some((21, "same", "k")), 21, "same", "k"),
            Offer::AlreadyInstalled
        );
    }

    #[test]
    fn another_bundle_for_the_installed_version_is_refused() {
        assert_eq!(
            judge_offer(Some((21, "mine", "k")), 21, "theirs", "k"),
            Offer::Refuse("conflicting bundle for v21".into())
        );
    }

    #[test]
    fn a_newer_offer_or_a_first_one_goes_to_install() {
        assert_eq!(
            judge_offer(Some((21, "a", "k")), 22, "b", "k"),
            Offer::Install
        );
        assert_eq!(judge_offer(None, 1, "b", "k"), Offer::Install);
    }

    #[test]
    fn a_bundle_from_another_key_is_refused_for_the_key_whatever_its_version() {
        let mismatch = Offer::Refuse(BundleError::KeyMismatch.to_string());
        assert_eq!(
            judge_offer(Some((21, "new", "ab")), 18, "old", "cd"),
            mismatch
        );
        assert_eq!(
            judge_offer(Some((21, "same", "ab")), 21, "same", "cd"),
            mismatch
        );
        assert_eq!(
            judge_offer(Some((21, "mine", "ab")), 21, "theirs", "cd"),
            mismatch
        );
        assert_eq!(judge_offer(Some((21, "a", "ab")), 22, "b", "cd"), mismatch);
        // The installed key is held as lowercase hex; an offer's case and
        // whitespace do not make it another key.
        assert_eq!(
            judge_offer(Some((21, "same", "ab")), 21, "same", " AB\n"),
            Offer::AlreadyInstalled
        );
    }

    #[test]
    fn a_verified_bundle_still_enforces_its_rules() {
        // Signing is only worth having if what it protects is the thing that
        // decides.
        let p: Policy = toml::from_str(WORKING_AGREEMENTS).unwrap();
        let d = p.decide(&Request {
            channel: "work",
            action: "bash",
            kind: Some("execute"),
            resource: None,
            command: Some("gh pr merge 12"),
            arguments: None,
            doc_kinds: None,
        });
        assert_eq!(d.verdict, Verdict::Deny);
    }
}
