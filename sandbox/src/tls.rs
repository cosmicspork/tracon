//! The node's identity and both ends' TLS configuration.
//!
//! The sandbox has no CA to consult and needs none: its bootstrap environment
//! carries the SHA-256 of the node's certificate, and the handshake succeeds
//! only against that certificate, proven by a signature with its key. Names
//! and validity dates are not checked; the pin is the whole identity.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring as provider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest as _, Sha256};

use crate::protocol::ALPN;

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("certificate pin must be 64 hex characters (optionally prefixed `sha256:`)")]
    BadPin,
    #[error("generate node identity: {0}")]
    Generate(String),
    #[error("tls configuration: {0}")]
    Config(#[from] rustls::Error),
    #[error("node identity: {0}")]
    Store(String),
}

/// The SHA-256 of a DER certificate.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CertPin([u8; 32]);

impl CertPin {
    pub fn of(cert_der: &[u8]) -> Self {
        CertPin(Sha256::digest(cert_der).into())
    }

    /// `sha256:<hex>` or bare hex, either case.
    pub fn parse(text: &str) -> Result<Self, TlsError> {
        let text = text.trim();
        let hex_part = text.strip_prefix("sha256:").unwrap_or(text);
        let bytes = hex::decode(hex_part).map_err(|_| TlsError::BadPin)?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| TlsError::BadPin)?;
        Ok(CertPin(bytes))
    }

    fn matches(&self, cert_der: &[u8]) -> bool {
        let other = CertPin::of(cert_der);
        // Not secret, but there is no reason to leak how much of it matched.
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

impl std::fmt::Display for CertPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sha256:{}", hex::encode(self.0))
    }
}

impl std::fmt::Debug for CertPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CertPin({self})")
    }
}

/// The node's certificate and key for sandbox connections.
pub struct NodeIdentity {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl NodeIdentity {
    /// A fresh self-signed certificate (ECDSA P-256). Persisting it, so the
    /// pin survives a restart, is the caller's choice.
    pub fn generate() -> Result<Self, TlsError> {
        let certified = rcgen::generate_simple_self_signed(vec![
            crate::protocol::DEFAULT_SERVER_NAME.to_string(),
        ])
        .map_err(|e| TlsError::Generate(e.to_string()))?;
        Ok(NodeIdentity {
            cert: certified.cert.der().clone(),
            key: PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()),
        })
    }

    /// The identity kept in `dir` (`cert.der` and `key.der`), made and kept
    /// there first if the certificate is not: so the pin every sandbox holds
    /// still matches after the node restarts. `true` with one just made.
    ///
    /// The certificate is written last, so one present is one whose pin may
    /// have been handed out: a key missing beside it is an error, never a
    /// reason to make another. Removing both is how the identity is
    /// rotated; sandboxes holding the old pin then refuse the node.
    pub fn load_or_generate(dir: &std::path::Path) -> Result<(Self, bool), TlsError> {
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
        let store = |what: &str, path: &std::path::Path, e: std::io::Error| {
            TlsError::Store(format!("{what} {}: {e}", path.display()))
        };
        let (cert_path, key_path) = (dir.join("cert.der"), dir.join("key.der"));
        match std::fs::read(&cert_path) {
            Ok(cert) => {
                let key = std::fs::read(&key_path).map_err(|e| store("read", &key_path, e))?;
                let identity = NodeIdentity::from_der(cert, key);
                // The pair must still make a server: a key that does not
                // match the certificate is refused here, not at a handshake.
                identity.server_config()?;
                return Ok((identity, false));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(store("read", &cert_path, error)),
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| store("create", dir, e))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| store("restrict", dir, e))?;
        let identity = NodeIdentity::generate()?;
        for (path, bytes) in [
            (&key_path, identity.key_der()),
            (&cert_path, identity.cert_der()),
        ] {
            let staged = path.with_extension("tmp");
            let _ = std::fs::remove_file(&staged);
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&staged)
                .map_err(|e| store("create", &staged, e))?;
            std::io::Write::write_all(&mut file, bytes).map_err(|e| store("write", &staged, e))?;
            file.sync_all().map_err(|e| store("sync", &staged, e))?;
            std::fs::rename(&staged, path).map_err(|e| store("keep", path, e))?;
        }
        Ok((identity, true))
    }

    pub fn from_der(cert: Vec<u8>, pkcs8_key: Vec<u8>) -> Self {
        NodeIdentity {
            cert: CertificateDer::from(cert),
            key: PrivatePkcs8KeyDer::from(pkcs8_key),
        }
    }

    pub fn cert_der(&self) -> &[u8] {
        self.cert.as_ref()
    }

    pub fn key_der(&self) -> &[u8] {
        self.key.secret_pkcs8_der()
    }

    /// What goes into a sandbox's bootstrap environment.
    pub fn pin(&self) -> CertPin {
        CertPin::of(self.cert.as_ref())
    }

    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>, TlsError> {
        let mut config =
            rustls::ServerConfig::builder_with_provider(Arc::new(provider::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13])?
                .with_no_client_auth()
                .with_single_cert(
                    vec![self.cert.clone()],
                    PrivateKeyDer::Pkcs8(self.key.clone_key()),
                )?;
        config.alpn_protocols = vec![ALPN.to_vec()];
        Ok(Arc::new(config))
    }
}

impl Clone for NodeIdentity {
    fn clone(&self) -> Self {
        NodeIdentity {
            cert: self.cert.clone(),
            key: self.key.clone_key(),
        }
    }
}

impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("pin", &self.pin())
            .finish_non_exhaustive()
    }
}

/// The sandbox's TLS configuration: TLS 1.3, the protocol's ALPN, and the
/// node's certificate accepted only if it hashes to `pin`.
pub fn client_config(pin: CertPin) -> Result<Arc<rustls::ClientConfig>, TlsError> {
    let provider = Arc::new(provider::default_provider());
    let verifier = PinVerifier {
        pin,
        algorithms: provider.signature_verification_algorithms,
    };
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct PinVerifier {
    pin: CertPin,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self.pin.matches(end_entity.as_ref()) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // The configuration offers TLS 1.3 only.
        Err(rustls::Error::General("TLS 1.2 is not offered".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // The pin names the certificate; this is what proves the peer holds
        // its key rather than having copied it.
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_parses_its_own_display() {
        let identity = NodeIdentity::generate().unwrap();
        let pin = identity.pin();
        assert_eq!(CertPin::parse(&pin.to_string()).unwrap(), pin);
        assert_eq!(
            CertPin::parse(&hex::encode(pin.0).to_uppercase()).unwrap(),
            pin
        );
        assert!(CertPin::parse("sha256:abcd").is_err());
        assert!(CertPin::parse("not hex at all").is_err());
        assert!(pin.matches(identity.cert_der()));
        assert!(!pin.matches(b"something else"));
    }

    #[test]
    fn a_kept_identity_keeps_its_pin_until_removed() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("sandbox-identity");
        let (made, created) = NodeIdentity::load_or_generate(&dir).unwrap();
        assert!(created);
        for name in ["cert.der", "key.der"] {
            let mode = std::fs::metadata(dir.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let (loaded, created) = NodeIdentity::load_or_generate(&dir).unwrap();
        assert!(!created);
        assert_eq!(loaded.pin(), made.pin());
        // A certificate whose key went missing is not quietly replaced.
        std::fs::remove_file(dir.join("key.der")).unwrap();
        assert!(NodeIdentity::load_or_generate(&dir).is_err());
        // Nor is one whose key is another's.
        std::fs::write(
            dir.join("key.der"),
            NodeIdentity::generate().unwrap().key_der(),
        )
        .unwrap();
        assert!(NodeIdentity::load_or_generate(&dir).is_err());
        // Removing both rotates it.
        std::fs::remove_dir_all(&dir).unwrap();
        let (rotated, created) = NodeIdentity::load_or_generate(&dir).unwrap();
        assert!(created);
        assert_ne!(rotated.pin(), made.pin());
    }
}
