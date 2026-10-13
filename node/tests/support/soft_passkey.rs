//! A software passkey: an ES256 authenticator that answers the node's own
//! WebAuthn options the way a browser and a platform authenticator would,
//! so the real verifier (`webauthn-rs`) checks real signatures over real
//! CBOR. Each knob a browser or authenticator controls (the origin it
//! reports, user verification, the counter) is a field, so a test can make
//! the one thing it is about wrong.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use p256::{
    ecdsa::{signature::Signer, Signature, SigningKey},
    SecretKey,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const UP: u8 = 0x01;
const UV: u8 = 0x04;
const BE: u8 = 0x08;
const BS: u8 = 0x10;
const AT: u8 = 0x40;

pub struct SoftPasskey {
    key: SigningKey,
    pub credential_id: Vec<u8>,
    /// The origin the "browser" puts in client data.
    pub origin: String,
    /// Whether the authenticator reports that it verified the user.
    pub user_verified: bool,
    pub counter: u32,
    user_handle: Option<Vec<u8>>,
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(s: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .expect("base64url")
}

// The few CBOR shapes a COSE key and an attestation object need.
fn head(major: u8, n: u64, out: &mut Vec<u8>) {
    let m = major << 5;
    match n {
        0..=23 => out.push(m | n as u8),
        24..=0xff => out.extend([m | 24, n as u8]),
        0x100..=0xffff => {
            out.push(m | 25);
            out.extend((n as u16).to_be_bytes());
        }
        _ => {
            out.push(m | 26);
            out.extend((n as u32).to_be_bytes());
        }
    }
}

fn int(i: i64, out: &mut Vec<u8>) {
    if i >= 0 {
        head(0, i as u64, out)
    } else {
        head(1, (-1 - i) as u64, out)
    }
}

fn bytes(b: &[u8], out: &mut Vec<u8>) {
    head(2, b.len() as u64, out);
    out.extend(b);
}

fn text(s: &str, out: &mut Vec<u8>) {
    head(3, s.len() as u64, out);
    out.extend(s.as_bytes());
}

impl SoftPasskey {
    /// A new authenticator for `origin`, its key derived from `seed`.
    pub fn new(seed: u8, origin: &str) -> Self {
        let mut raw = [0u8; 32];
        raw[0] = 1;
        raw[31] = seed;
        let secret = SecretKey::from_slice(&raw).expect("a valid scalar");
        Self {
            key: SigningKey::from(&secret),
            credential_id: Sha256::digest([b"cred".as_slice(), &[seed]].concat()).to_vec(),
            origin: origin.to_string(),
            user_verified: true,
            counter: 0,
            user_handle: None,
        }
    }

    pub fn id(&self) -> String {
        b64(&self.credential_id)
    }

    fn cose_key(&self) -> Vec<u8> {
        let point = self.key.verifying_key().to_encoded_point(false);
        let mut out = Vec::new();
        head(5, 5, &mut out);
        int(1, &mut out);
        int(2, &mut out); // kty: EC2
        int(3, &mut out);
        int(-7, &mut out); // alg: ES256
        int(-1, &mut out);
        int(1, &mut out); // crv: P-256
        int(-2, &mut out);
        bytes(point.x().unwrap(), &mut out);
        int(-3, &mut out);
        bytes(point.y().unwrap(), &mut out);
        out
    }

    fn flags(&self) -> u8 {
        UP | BE | BS | if self.user_verified { UV } else { 0 }
    }

    fn client_data(&self, kind: &str, challenge: &str) -> Vec<u8> {
        json!({
            "type": kind,
            "challenge": challenge,
            "origin": self.origin,
            "crossOrigin": false,
        })
        .to_string()
        .into_bytes()
    }

    /// Answer `navigator.credentials.create` with the node's options (the
    /// `options` object the register route returned).
    pub fn create(&mut self, options: &Value) -> Value {
        let pk = &options["publicKey"];
        let rp_id = pk["rp"]["id"].as_str().expect("rp.id");
        let challenge = pk["challenge"].as_str().expect("challenge");
        self.user_handle = pk["user"]["id"].as_str().map(unb64);

        let mut auth_data = Sha256::digest(rp_id.as_bytes()).to_vec();
        auth_data.push(self.flags() | AT);
        auth_data.extend(self.counter.to_be_bytes());
        auth_data.extend([0u8; 16]); // AAGUID: none
        auth_data.extend((self.credential_id.len() as u16).to_be_bytes());
        auth_data.extend(&self.credential_id);
        auth_data.extend(self.cose_key());

        let mut attestation = Vec::new();
        head(5, 3, &mut attestation);
        text("fmt", &mut attestation);
        text("none", &mut attestation);
        text("attStmt", &mut attestation);
        head(5, 0, &mut attestation);
        text("authData", &mut attestation);
        bytes(&auth_data, &mut attestation);

        json!({
            "id": self.id(),
            "rawId": self.id(),
            "type": "public-key",
            "response": {
                "attestationObject": b64(&attestation),
                "clientDataJSON": b64(&self.client_data("webauthn.create", challenge)),
                "transports": ["internal", "hybrid"],
            },
            "extensions": {},
        })
    }

    /// Answer `navigator.credentials.get` with the node's options.
    pub fn get(&mut self, options: &Value) -> Value {
        let pk = &options["publicKey"];
        let rp_id = pk["rpId"].as_str().expect("rpId");
        let challenge = pk["challenge"].as_str().expect("challenge");
        self.counter += 1;
        let mut auth_data = Sha256::digest(rp_id.as_bytes()).to_vec();
        auth_data.push(self.flags());
        auth_data.extend(self.counter.to_be_bytes());
        let client_data = self.client_data("webauthn.get", challenge);
        let mut signed = auth_data.clone();
        signed.extend(Sha256::digest(&client_data));
        let signature: Signature = self.key.sign(&signed);
        json!({
            "id": self.id(),
            "rawId": self.id(),
            "type": "public-key",
            "response": {
                "authenticatorData": b64(&auth_data),
                "clientDataJSON": b64(&client_data),
                "signature": b64(signature.to_der().as_bytes()),
                "userHandle": self.user_handle.as_deref().map(b64),
            },
            "extensions": {},
        })
    }
}
