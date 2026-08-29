//! Web Push without a relay: RFC 8291 payload encryption and RFC 8292 VAPID,
//! over the node's own HTTP client.
//!
//! The push service (Apple's, Google's, Mozilla's) sees a ciphertext it cannot
//! open and a signature that names this node; the phone's service worker sees
//! the title and the path. No third party of ours sits in between, and no
//! device key ever leaves the browser: the node holds only the subscription's
//! public half.

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes128Gcm, Nonce,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hkdf::Hkdf;
use p256::{
    ecdsa::{signature::Signer, Signature, SigningKey},
    elliptic_curve::sec1::ToEncodedPoint,
    PublicKey, SecretKey,
};
use sha2::Sha256;

use crate::store::Store;

/// Where the node keeps its VAPID key. Node-local by construction: `kv` is not
/// a replicated table.
pub const VAPID_KEY: &str = "vapid_secret";
/// Push services cap a payload at 4 KiB; one record holds everything.
const RECORD_SIZE: u32 = 4096;
/// The last-record delimiter RFC 8188 puts after the plaintext.
const LAST_RECORD: u8 = 0x02;
/// A VAPID token may live a day; half that leaves clock skew room to spare.
const TOKEN_LIFE_SECS: i64 = 12 * 60 * 60;

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("subscription key is not a P-256 point")]
    BadKey,
    #[error("subscription auth secret is not 16 bytes")]
    BadAuth,
    #[error("payload too large for one record")]
    TooLarge,
    #[error("encryption failed")]
    Seal,
    #[error("malformed push payload")]
    Malformed,
    #[error("endpoint is not an https URL")]
    BadEndpoint,
}

/// The node's signing identity towards push services.
pub struct Vapid {
    secret: SecretKey,
}

impl Vapid {
    /// The key in the store, generated the first time it is asked for.
    pub fn load_or_generate(store: &Store) -> Self {
        if let Ok(Some(hex_key)) = store.kv_get(VAPID_KEY) {
            if let Ok(bytes) = hex::decode(hex_key) {
                if let Ok(secret) = SecretKey::from_slice(&bytes) {
                    return Self { secret };
                }
            }
            tracing::warn!(
                "stored VAPID key unreadable; generating a new one (devices must resubscribe)"
            );
        }
        let secret = random_secret();
        let _ = store.kv_put(VAPID_KEY, &hex::encode(secret.to_bytes()));
        Self { secret }
    }

    #[cfg(test)]
    pub fn from_secret(secret: SecretKey) -> Self {
        Self { secret }
    }

    /// The uncompressed public point, as `applicationServerKey` wants it.
    pub fn public_key_b64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(public_bytes(&self.secret.public_key()))
    }

    pub fn public_key(&self) -> PublicKey {
        self.secret.public_key()
    }

    /// The `Authorization` header for one push service origin.
    fn authorization(&self, audience: &str, subject: &str, now_secs: i64) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({
            "aud": audience,
            "exp": now_secs + TOKEN_LIFE_SECS,
            "sub": subject,
        });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signing_input = format!("{header}.{claims}");
        let signer = SigningKey::from(&self.secret);
        // ES256 in a JWT is the raw r‖s pair, never DER.
        let sig: Signature = signer.sign(signing_input.as_bytes());
        let token = format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()));
        format!("vapid t={token}, k={}", self.public_key_b64url())
    }
}

fn random_secret() -> SecretKey {
    loop {
        let bytes: [u8; 32] = rand::random();
        if let Ok(k) = SecretKey::from_slice(&bytes) {
            return k;
        }
    }
}

fn public_bytes(key: &PublicKey) -> [u8; 65] {
    let point = key.to_encoded_point(false);
    let mut out = [0u8; 65];
    out.copy_from_slice(point.as_bytes());
    out
}

/// One device, as the browser described it and the store kept it.
pub struct Subscriber<'a> {
    pub endpoint: &'a str,
    /// The device's P-256 public key, uncompressed.
    pub p256dh: &'a [u8],
    /// The device's 16-byte auth secret.
    pub auth: &'a [u8],
}

/// Decode what `PushSubscription.toJSON()` gave us, refusing the shapes a
/// push service would refuse later.
pub fn decode_keys(p256dh: &str, auth: &str) -> Result<([u8; 65], [u8; 16]), PushError> {
    let key = URL_SAFE_NO_PAD
        .decode(p256dh.trim_end_matches('='))
        .map_err(|_| PushError::BadKey)?;
    let key: [u8; 65] = key.try_into().map_err(|_| PushError::BadKey)?;
    PublicKey::from_sec1_bytes(&key).map_err(|_| PushError::BadKey)?;
    let auth = URL_SAFE_NO_PAD
        .decode(auth.trim_end_matches('='))
        .map_err(|_| PushError::BadAuth)?;
    let auth: [u8; 16] = auth.try_into().map_err(|_| PushError::BadAuth)?;
    Ok((key, auth))
}

/// Seal a payload for one device: a fresh salt and a fresh ephemeral key.
pub fn encrypt(sub: &Subscriber, plaintext: &[u8]) -> Result<Vec<u8>, PushError> {
    let salt: [u8; 16] = rand::random();
    encrypt_with(sub, plaintext, salt, random_secret())
}

/// RFC 8291 §3, with the randomness handed in so the specification's own
/// vector can be checked byte for byte.
pub(crate) fn encrypt_with(
    sub: &Subscriber,
    plaintext: &[u8],
    salt: [u8; 16],
    as_secret: SecretKey,
) -> Result<Vec<u8>, PushError> {
    if plaintext.len() + 1 + 16 > RECORD_SIZE as usize {
        return Err(PushError::TooLarge);
    }
    let ua_public = PublicKey::from_sec1_bytes(sub.p256dh).map_err(|_| PushError::BadKey)?;
    if sub.auth.len() != 16 {
        return Err(PushError::BadAuth);
    }
    let as_public = public_bytes(&as_secret.public_key());
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_public.as_affine());
    let (cek, nonce) = derive(
        sub.auth,
        shared.raw_secret_bytes(),
        sub.p256dh,
        &as_public,
        &salt,
    );

    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(LAST_RECORD);
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|_| PushError::Seal)?;
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &record,
                aad: &[],
            },
        )
        .map_err(|_| PushError::Seal)?;

    // The aes128gcm content-coding header: salt, record size, key id length,
    // and the sender's public key as the key id.
    let mut out = Vec::with_capacity(16 + 4 + 1 + 65 + sealed.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(65);
    out.extend_from_slice(&as_public);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// The device's side of the same derivation, for tests and the fake push
/// service they run against.
pub fn decrypt(ua_secret: &SecretKey, auth: &[u8], body: &[u8]) -> Result<Vec<u8>, PushError> {
    if body.len() < 16 + 4 + 1 + 65 + 16 {
        return Err(PushError::Malformed);
    }
    let salt = &body[..16];
    if body[20] != 65 {
        return Err(PushError::Malformed);
    }
    let as_public = &body[21..86];
    let sealed = &body[86..];
    let as_pub = PublicKey::from_sec1_bytes(as_public).map_err(|_| PushError::Malformed)?;
    let ua_public = public_bytes(&ua_secret.public_key());
    let shared = p256::ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), as_pub.as_affine());
    let (cek, nonce) = derive(auth, shared.raw_secret_bytes(), &ua_public, as_public, salt);
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|_| PushError::Malformed)?;
    let mut record = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: sealed,
                aad: &[],
            },
        )
        .map_err(|_| PushError::Malformed)?;
    // Strip the delimiter and any padding after it.
    let end = record
        .iter()
        .rposition(|b| *b != 0)
        .ok_or(PushError::Malformed)?;
    if record[end] != LAST_RECORD {
        return Err(PushError::Malformed);
    }
    record.truncate(end);
    Ok(record)
}

/// RFC 8291 §3.3–3.4: auth secret and ECDH secret into a content key and a
/// nonce, via two HKDF rounds.
fn derive(
    auth: &[u8],
    ecdh_secret: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8],
) -> ([u8; 16], [u8; 12]) {
    let mut key_info = Vec::with_capacity(14 + 65 + 65);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), ecdh_secret)
        .expand(&key_info, &mut ikm)
        .expect("32 bytes is a valid HKDF length");

    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .expect("16 bytes is a valid HKDF length");
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .expect("12 bytes is a valid HKDF length");
    (cek, nonce)
}

/// What one delivery attempt came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    /// The push service says this subscription no longer exists.
    Gone,
    /// Refused with a status that retrying will not change.
    Refused(u16),
    /// Worth one more try later.
    Unreachable,
}

/// The scheme and host of an endpoint, which is what the token is bound to.
/// Real push services are https; a loopback one (a test's stand-in) may be
/// plain http, since nothing on the wire leaves the machine.
pub fn audience(endpoint: &str) -> Result<String, PushError> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| PushError::BadEndpoint)?;
    let host = url.host_str().ok_or(PushError::BadEndpoint)?;
    let loopback = host
        .trim_matches(|c| c == '[' || c == ']')
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback());
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(PushError::BadEndpoint);
    }
    Ok(match url.port() {
        Some(p) => format!("{}://{host}:{p}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

/// One attempt at the push service. `topic` lets the service collapse
/// undelivered pushes that replace each other; `ttl_secs` bounds how long it
/// holds one for a phone that is off.
pub async fn send(
    http: &reqwest::Client,
    vapid: &Vapid,
    subject: &str,
    sub: &Subscriber<'_>,
    body: Vec<u8>,
    ttl_secs: u32,
    topic: &str,
) -> Outcome {
    let Ok(aud) = audience(sub.endpoint) else {
        return Outcome::Refused(0);
    };
    let now = crate::store::now_ms() / 1000;
    let req = http
        .post(sub.endpoint)
        .header("Authorization", vapid.authorization(&aud, subject, now))
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("TTL", ttl_secs.to_string())
        .header("Urgency", "high")
        .header("Topic", topic_of(topic))
        .body(body);
    match req.send().await {
        Ok(res) => match res.status().as_u16() {
            200..=299 => Outcome::Sent,
            404 | 410 => Outcome::Gone,
            429 | 500..=599 => Outcome::Unreachable,
            s => Outcome::Refused(s),
        },
        Err(_) => Outcome::Unreachable,
    }
}

/// A `Topic` is at most 32 URL-safe characters; tags are longer, so hash them
/// down while keeping equal tags equal.
fn topic_of(tag: &str) -> String {
    use sha2::Digest;
    let digest = Sha256::digest(tag.as_bytes());
    URL_SAFE_NO_PAD.encode(&digest[..24])
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{signature::Verifier, VerifyingKey};

    fn b64(s: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(s).unwrap()
    }

    /// RFC 8291 Appendix A, byte for byte.
    #[test]
    fn the_specifications_own_example_encrypts_to_its_own_bytes() {
        let ua_secret =
            SecretKey::from_slice(&b64("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94")).unwrap();
        let ua_public = b64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        assert_eq!(public_bytes(&ua_secret.public_key()).to_vec(), ua_public);
        let auth = b64("BTBZMqHH6r4Tts7J_aSIgg");
        let as_secret =
            SecretKey::from_slice(&b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let sub = Subscriber {
            endpoint: "https://push.example/x",
            p256dh: &ua_public,
            auth: &auth,
        };
        let out = encrypt_with(
            &sub,
            b"When I grow up, I want to be a watermelon",
            salt,
            as_secret,
        )
        .unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD.encode(&out),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
        assert_eq!(
            decrypt(&ua_secret, &auth, &out).unwrap(),
            b"When I grow up, I want to be a watermelon"
        );
    }

    #[test]
    fn a_fresh_seal_round_trips_and_differs_each_time() {
        let ua_secret = random_secret();
        let ua_public = public_bytes(&ua_secret.public_key());
        let auth: [u8; 16] = rand::random();
        let sub = Subscriber {
            endpoint: "https://push.example/x",
            p256dh: &ua_public,
            auth: &auth,
        };
        let a = encrypt(&sub, b"{\"title\":\"x\"}").unwrap();
        let b = encrypt(&sub, b"{\"title\":\"x\"}").unwrap();
        assert_ne!(a, b, "salt and ephemeral key are fresh per push");
        assert_eq!(
            decrypt(&ua_secret, &auth, &a).unwrap(),
            b"{\"title\":\"x\"}"
        );
        assert!(
            decrypt(&ua_secret, &[0u8; 16], &a).is_err(),
            "the wrong auth secret opens nothing"
        );
        let big = vec![b'x'; 5000];
        assert!(matches!(encrypt(&sub, &big), Err(PushError::TooLarge)));
    }

    #[test]
    fn the_vapid_token_verifies_and_names_the_audience() {
        let v = Vapid::from_secret(random_secret());
        let header = v.authorization("https://push.example", "mailto:x@y", 1_000);
        let (t, k) = header
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        assert_eq!(k, v.public_key_b64url());
        let parts: Vec<&str> = t.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: serde_json::Value = serde_json::from_slice(&b64(parts[1])).unwrap();
        assert_eq!(claims["aud"], "https://push.example");
        assert_eq!(claims["exp"], 1_000 + TOKEN_LIFE_SECS);
        let sig = b64(parts[2]);
        assert_eq!(sig.len(), 64, "raw r||s, not DER");
        let sig = Signature::from_slice(&sig).unwrap();
        let vk = VerifyingKey::from(&v.public_key());
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .expect("the token is signed by the advertised key");
    }

    #[test]
    fn subscription_keys_are_checked_on_the_way_in() {
        assert!(decode_keys("AAAA", "BTBZMqHH6r4Tts7J_aSIgg").is_err());
        assert!(decode_keys(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
            "short"
        )
        .is_err());
        // Browsers pad or do not; both spellings are accepted.
        assert!(decode_keys(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
            "BTBZMqHH6r4Tts7J_aSIgg=="
        )
        .is_ok());
        assert_eq!(
            audience("https://web.push.apple.com/abc").unwrap(),
            "https://web.push.apple.com"
        );
        assert!(audience("http://push.example/x").is_err());
        assert_eq!(
            audience("http://127.0.0.1:9/push").unwrap(),
            "http://127.0.0.1:9"
        );
        assert!(topic_of("tracon-perm-1234567890").len() <= 32);
        assert_eq!(topic_of("a"), topic_of("a"));
    }
}
