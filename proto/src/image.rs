//! Image statements: a node's signed word that the image it built from a
//! repository's recipe is in a registry under one digest.
//!
//! ```text
//! { repo, recipe, platform, image, signer, signed_ms, sig }
//! ```
//!
//! A repository's recipe is shared between nodes; the image each builds from
//! it is not, because a local build has a digest of its own on every node.
//! A node that pushed what it built to a registry its channel co-members can
//! pull from says so to them with one of these, and a co-member that accepted
//! the same recipe may pull that image instead of building it. It takes the
//! image only by the digest the statement signs, so the registry cannot hand
//! it anything else, and only from a node it shares a channel with.
//!
//! - `repo`: the repository's forge identity, `host/owner/name`.
//! - `recipe`: the recipe's hash as the node keys its build on it, 64
//!   lower-case hex characters. Computed alike by any node from the same
//!   recipe and the same pinned stock base.
//! - `platform`: `linux/<arch>` the image was built for.
//! - `image`: `name@sha256:<64 hex>` in the registry.
//! - `signer`: the node id (Ed25519 public key, hex) that built and pushed it.
//! - `signed_ms`: when it signed.
//! - `sig`: Ed25519 by `signer` over [`ImageStatement::signing_bytes`].

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::keys::{key32, Identity};
use crate::put_str;

/// The longest image reference a statement carries.
pub const MAX_IMAGE: usize = 512;

/// The longest forge identity a statement carries.
pub const MAX_REPO: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageStatement {
    pub repo: String,
    pub recipe: String,
    pub platform: String,
    pub image: String,
    pub signer: String,
    pub signed_ms: i64,
    pub sig: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImageStatementError {
    #[error("malformed image statement field: {0}")]
    Malformed(&'static str),
    #[error("the image statement's signature does not verify")]
    BadSignature,
}

fn lower_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `image` is `name@sha256:<64 lower-case hex>`, with no whitespace
/// or control character in it.
fn pinned(image: &str) -> bool {
    match image.rsplit_once("@sha256:") {
        Some((name, hex)) => {
            !name.is_empty()
                && image.len() <= MAX_IMAGE
                && !image.chars().any(|c| c.is_whitespace() || c.is_control())
                && lower_hex(hex, 64)
        }
        None => false,
    }
}

impl ImageStatement {
    /// Sign a statement as `signer` that `image` is what it built from
    /// `recipe` of `repo` for `platform`.
    pub fn sign(
        signer: &Identity,
        repo: &str,
        recipe: &str,
        platform: &str,
        image: &str,
        signed_ms: i64,
    ) -> Self {
        let mut statement = ImageStatement {
            repo: repo.to_string(),
            recipe: recipe.to_string(),
            platform: platform.to_string(),
            image: image.to_string(),
            signer: signer.node_id(),
            signed_ms,
            sig: String::new(),
        };
        statement.sig = hex::encode(signer.sign(&statement.signing_bytes()).to_bytes());
        statement
    }

    /// What the signer signs: every field but the signature, each
    /// length-prefixed, under this contract's domain label.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = crate::version_label("image-statement").into_bytes();
        put_str(&mut out, &self.repo);
        put_str(&mut out, &self.recipe);
        put_str(&mut out, &self.platform);
        put_str(&mut out, &self.image);
        put_str(&mut out, &self.signer);
        out.extend_from_slice(&self.signed_ms.to_be_bytes());
        out
    }

    /// The digest the statement pins, `sha256:<hex>`.
    pub fn digest(&self) -> &str {
        self.image
            .rsplit_once('@')
            .map(|(_, digest)| digest)
            .unwrap_or_default()
    }

    /// Whether `signer`'s key signed this statement as it stands, and its
    /// fields have the shapes they must. Says nothing about whether the
    /// signer is a node to take an image from: the receiver checks that.
    pub fn verify(&self) -> Result<(), ImageStatementError> {
        let signer = key32(&self.signer).ok_or(ImageStatementError::Malformed("signer"))?;
        let identity = self.repo.split('/').collect::<Vec<_>>();
        if self.repo.len() > MAX_REPO
            || identity.len() != 3
            || identity.iter().any(|part| {
                part.is_empty()
                    || part
                        .chars()
                        .any(|c| c.is_whitespace() || c.is_control() || c == '@' || c == ':')
            })
        {
            return Err(ImageStatementError::Malformed("repo"));
        }
        if !lower_hex(&self.recipe, 64) {
            return Err(ImageStatementError::Malformed("recipe"));
        }
        match self.platform.strip_prefix("linux/") {
            Some(arch)
                if !arch.is_empty()
                    && arch.len() <= 32
                    && arch
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) => {}
            _ => return Err(ImageStatementError::Malformed("platform")),
        }
        if !pinned(&self.image) {
            return Err(ImageStatementError::Malformed("image"));
        }
        let sig: [u8; 64] = hex::decode(&self.sig)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or(ImageStatementError::Malformed("sig"))?;
        let vk = VerifyingKey::from_bytes(&signer)
            .map_err(|_| ImageStatementError::Malformed("signer"))?;
        if !crate::keys::verify(&vk, &self.signing_bytes(), &Signature::from_bytes(&sig)) {
            return Err(ImageStatementError::BadSignature);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!("../../spec/vectors/image-statement.json");

    #[derive(Deserialize)]
    struct VectorFile {
        contract_version: u32,
        signer_seed_hex: String,
        statement: ImageStatement,
        signing_bytes_hex: String,
    }

    fn statement() -> ImageStatement {
        ImageStatement::sign(
            &Identity::from_seed(&[7u8; 32]),
            "github.com/acme/app",
            &"ab".repeat(32),
            "linux/amd64",
            &format!(
                "registry.digitalocean.com/acme/tracon-repo-images@sha256:{}",
                "cd".repeat(32)
            ),
            1_787_000_000_000,
        )
    }

    #[test]
    fn a_statement_verifies_only_as_signed() {
        let s = statement();
        s.verify().unwrap();
        assert_eq!(s.digest(), format!("sha256:{}", "cd".repeat(32)));
        let round: ImageStatement =
            serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        round.verify().unwrap();

        // Another digest, another node, another recipe: none of them is what
        // the signer said.
        let mut t = s.clone();
        t.image = t.image.replace("cdcd", "cdce");
        assert_eq!(t.verify(), Err(ImageStatementError::BadSignature));
        let mut t = s.clone();
        t.signer = Identity::from_seed(&[8u8; 32]).node_id();
        assert_eq!(t.verify(), Err(ImageStatementError::BadSignature));
        let mut t = s.clone();
        t.recipe = "ef".repeat(32);
        assert_eq!(t.verify(), Err(ImageStatementError::BadSignature));

        // Shapes are refused before the signature is looked at.
        for (field, edit) in [
            (
                "image",
                (|t: &mut ImageStatement| t.image = "x:latest".into()) as fn(&mut _),
            ),
            ("image", |t| {
                t.image = format!("a b@sha256:{}", "cd".repeat(32))
            }),
            ("recipe", |t| t.recipe = "AB".repeat(32)),
            ("platform", |t| t.platform = "windows/amd64".into()),
            ("repo", |t| t.repo = "acme/app".into()),
            ("sig", |t| t.sig = "00".into()),
        ] {
            let mut t = s.clone();
            edit(&mut t);
            assert_eq!(t.verify(), Err(ImageStatementError::Malformed(field)));
        }
        // Nothing but the statement's own fields is read.
        let mut extra = serde_json::to_value(&s).unwrap();
        extra["tag"] = "latest".into();
        assert!(serde_json::from_value::<ImageStatement>(extra).is_err());
    }

    #[test]
    fn matches_spec_vectors() {
        let f: VectorFile = serde_json::from_str(VECTORS).unwrap();
        assert_eq!(f.contract_version, crate::CONTRACT_VERSION);
        let signer = Identity::from_seed(&key32(&f.signer_seed_hex).unwrap());
        let s = ImageStatement::sign(
            &signer,
            &f.statement.repo,
            &f.statement.recipe,
            &f.statement.platform,
            &f.statement.image,
            f.statement.signed_ms,
        );
        assert_eq!(hex::encode(s.signing_bytes()), f.signing_bytes_hex);
        assert_eq!(s, f.statement);
        s.verify().unwrap();
    }
}
