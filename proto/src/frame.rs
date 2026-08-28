//! The mesh frame: what nodes post to the hub and pull from it.
//!
//! ```text
//! { v, id, channel, sender, recipient?, sealing, sent_ms, body, sig }
//! ```
//!
//! - `channel` is the routing key; the hub reads it and nothing else.
//! - `sender` is the node id (Ed25519 public key, hex); the frame is signed by it.
//! - `sealing` is `Channel { epoch }` (body sealed under the channel's epoch key,
//!   readable by every member) or `Direct` (body sealed to `recipient`'s X25519
//!   key, readable by that node only).
//! - `id` is `SHA256(domain ‖ canonical bytes)`; `sig` covers the id.
//!
//! **Sealed then signed.** The hub and every peer verify authorship before any
//! decrypt path runs; a tampered frame is dropped without being opened. The
//! sealing metadata (channel, epoch or sender/recipient) is bound into the AEAD
//! associated data, so a frame the hub re-labels onto another channel fails to
//! open there.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use x25519_dalek::PublicKey;

use crate::envelope::{seal_to, EnvelopeError, Sealed, SealedBox};
use crate::keyring::{channel_aad, Keyring, EPOCH_ID_LEN};
use crate::keys::{key32, Identity};
use crate::{put_bytes, put_str};

/// The channel every node is a member of: presence, enrollment handoffs,
/// policy distribution.
pub const MESH_CHANNEL: &str = "@mesh";

/// Hard cap on a serialized frame the hub will accept.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Version-independent domain tag for the frame id, so an id is stable across
/// contract builds.
const DOMAIN_FRAME_ID: &[u8] = b"tracon/frame-id\0";

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("malformed frame field: {0}")]
    Malformed(&'static str),
    #[error("frame id does not match its contents")]
    BadId,
    #[error("frame signature does not verify")]
    BadSignature,
    #[error("frame is not sealed the way this open expects")]
    WrongSealing,
    #[error("frame is not addressed to this node")]
    NotRecipient,
    #[error("no key for epoch {0}")]
    UnknownEpoch(String),
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    #[error("payload is not valid JSON: {0}")]
    Payload(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Sealing {
    Channel { epoch: String },
    Direct,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub id: String,
    pub channel: String,
    pub sender: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    pub sealing: Sealing,
    pub sent_ms: i64,
    /// base64 (standard, padded) of the sealed bytes.
    pub body: String,
    pub sig: String,
}

/// Is `name` an acceptable channel name? Lowercase, digits, `@._-`, 1–64 chars.
/// `AAD_SEP` (0x1f) is outside this set by construction.
pub fn valid_channel(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"@._-".contains(&b))
}

/// The plaintext inside a frame. Rows travel as JSON values so this crate has
/// no dependency on node types.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Payload {
    /// Presence, on `@mesh`. `node` is the node's public description.
    Hello {
        node: Value,
        contract: u32,
    },
    /// The sender's full open state on one channel.
    Snapshot {
        sessions: Vec<Value>,
        waiting: Vec<Value>,
        reviews: Vec<Value>,
    },
    Session(Value),
    Event {
        origin_seq: i64,
        session_id: String,
        event_kind: String,
        #[serde(default)]
        ref_id: Option<String>,
        payload: Value,
        at_ms: i64,
    },
    Queue {
        waiting: Vec<Value>,
    },
    Reviews {
        waiting: Vec<Value>,
    },
    Node(Value),
    Command {
        cmd_id: String,
        command: Command,
    },
    Ack {
        cmd_id: String,
        ok: Option<Value>,
        err: Option<String>,
    },
    EventsRequest {
        session_id: String,
        after_origin_seq: i64,
    },
    EventsBatch {
        session_id: String,
        events: Vec<Value>,
        done: bool,
    },
    /// Direct only.
    KeyHandoff {
        channels: Vec<ChannelHandoff>,
    },
    /// Direct only.
    PolicyBundle {
        toml: String,
        sig_hex: String,
        pubkey_hex: String,
    },
    /// Direct only: credentials for the recipient's broker, each
    /// `{ "name": …, "credential": … }` in the broker's own row shape. The
    /// receiver keeps its own bindings check; a credential not bound to it is
    /// dropped there.
    CredentialHandoff {
        credentials: Vec<Value>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    Create {
        spec: Value,
    },
    Prompt {
        session_id: String,
        text: String,
    },
    Answer {
        permission_id: String,
        option_id: String,
    },
    Kill {
        session_id: String,
    },
    Verdict {
        review_id: String,
        verdict: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        body: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelHandoff {
    pub name: String,
    /// A [`Keyring`] wrapped to the recipient, as hex container bytes.
    pub keyring: Keyring,
    #[serde(default = "default_bindings")]
    pub bindings_json: String,
}

fn default_bindings() -> String {
    "{}".into()
}

impl PartialEq for Keyring {
    fn eq(&self, other: &Self) -> bool {
        self.to_bytes() == other.to_bytes()
    }
}

/// Everything about a frame that is not the body: used to build one and, on
/// receipt, to recompute its id.
struct Header<'a> {
    channel: &'a str,
    sender: [u8; 32],
    recipient: Option<[u8; 32]>,
    epoch: Option<[u8; EPOCH_ID_LEN]>,
    sent_ms: i64,
}

fn canonical_bytes(h: &Header, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&crate::CONTRACT_VERSION.to_be_bytes());
    put_str(&mut out, h.channel);
    out.extend_from_slice(&h.sender);
    match h.recipient {
        Some(r) => {
            out.push(1);
            out.extend_from_slice(&r);
        }
        None => out.push(0),
    }
    match h.epoch {
        Some(e) => {
            out.push(1);
            out.extend_from_slice(&e);
        }
        None => out.push(0),
    }
    out.extend_from_slice(&h.sent_ms.to_be_bytes());
    put_bytes(&mut out, body);
    out
}

fn frame_id(canon: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(DOMAIN_FRAME_ID);
    h.update(canon);
    h.finalize().into()
}

fn signing_bytes(id: &[u8; 32]) -> Vec<u8> {
    let mut out = crate::version_label("frame").into_bytes();
    out.extend_from_slice(id);
    out
}

fn direct_aad(channel: &str, sender: &[u8; 32], recipient: &[u8; 32]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(channel.len() + 65);
    aad.extend_from_slice(channel.as_bytes());
    aad.push(crate::keyring::AAD_SEP);
    aad.extend_from_slice(sender);
    aad.extend_from_slice(recipient);
    aad
}

impl Envelope {
    fn assemble(sender: &Identity, h: Header, body: Vec<u8>) -> Envelope {
        use base64::Engine;
        let id = frame_id(&canonical_bytes(&h, &body));
        let sig = sender.sign(&signing_bytes(&id));
        Envelope {
            v: crate::CONTRACT_VERSION,
            id: hex::encode(id),
            channel: h.channel.to_string(),
            sender: hex::encode(h.sender),
            recipient: h.recipient.map(hex::encode),
            sealing: match h.epoch {
                Some(e) => Sealing::Channel {
                    epoch: hex::encode(e),
                },
                None => Sealing::Direct,
            },
            sent_ms: h.sent_ms,
            body: base64::engine::general_purpose::STANDARD.encode(body),
            sig: hex::encode(sig.to_bytes()),
        }
    }

    /// Seal `payload` under the newest epoch of `keyring` (which must be wrapped
    /// to `sender`). `recipient` is a routing hint for addressed payloads that
    /// every member may still read (commands, acks).
    pub fn seal_channel(
        sender: &Identity,
        channel: &str,
        recipient: Option<&str>,
        keyring: &Keyring,
        payload: &Payload,
        sent_ms: i64,
    ) -> Result<Envelope, FrameError> {
        let entry = keyring.newest();
        let key = keyring.key_for(entry, sender)?;
        let plaintext = serde_json::to_vec(payload)?;
        let body = key
            .seal(&plaintext, &channel_aad(channel, &entry.id()))
            .to_bytes();
        Ok(Self::assemble(
            sender,
            Header {
                channel,
                sender: sender.verifying_key().to_bytes(),
                recipient: match recipient {
                    Some(r) => Some(key32(r).ok_or(FrameError::Malformed("recipient"))?),
                    None => None,
                },
                epoch: Some(entry.id()),
                sent_ms,
            },
            body,
        ))
    }

    /// Seal `payload` to one node's X25519 key. Only that node can open it; the
    /// hub relays ciphertext.
    pub fn seal_direct(
        sender: &Identity,
        channel: &str,
        recipient_node_id: &str,
        recipient_x25519: &PublicKey,
        payload: &Payload,
        sent_ms: i64,
    ) -> Result<Envelope, FrameError> {
        let recipient = key32(recipient_node_id).ok_or(FrameError::Malformed("recipient"))?;
        let s = sender.verifying_key().to_bytes();
        let plaintext = serde_json::to_vec(payload)?;
        let body = seal_to(
            recipient_x25519,
            &plaintext,
            &direct_aad(channel, &s, &recipient),
        )
        .to_bytes();
        Ok(Self::assemble(
            sender,
            Header {
                channel,
                sender: s,
                recipient: Some(recipient),
                epoch: None,
                sent_ms,
            },
            body,
        ))
    }

    fn parsed(&self) -> Result<(Header<'_>, Vec<u8>), FrameError> {
        use base64::Engine;
        if !valid_channel(&self.channel) {
            return Err(FrameError::Malformed("channel"));
        }
        let sender = key32(&self.sender).ok_or(FrameError::Malformed("sender"))?;
        let recipient = match &self.recipient {
            Some(r) => Some(key32(r).ok_or(FrameError::Malformed("recipient"))?),
            None => None,
        };
        let epoch = match &self.sealing {
            Sealing::Channel { epoch } => {
                let e: [u8; EPOCH_ID_LEN] = hex::decode(epoch)
                    .ok()
                    .and_then(|b| b.try_into().ok())
                    .ok_or(FrameError::Malformed("epoch"))?;
                Some(e)
            }
            Sealing::Direct => {
                if recipient.is_none() {
                    return Err(FrameError::Malformed("direct frame without recipient"));
                }
                None
            }
        };
        let body = base64::engine::general_purpose::STANDARD
            .decode(&self.body)
            .map_err(|_| FrameError::Malformed("body"))?;
        Ok((
            Header {
                channel: &self.channel,
                sender,
                recipient,
                epoch,
                sent_ms: self.sent_ms,
            },
            body,
        ))
    }

    /// Recompute the id from the contents and check the signature. Returns the
    /// sender's key on success. Never trusts the stored id.
    pub fn verify(&self) -> Result<[u8; 32], FrameError> {
        if self.v != crate::CONTRACT_VERSION {
            return Err(FrameError::Malformed("v"));
        }
        let (h, body) = self.parsed()?;
        let id = frame_id(&canonical_bytes(&h, &body));
        if hex::encode(id) != self.id {
            return Err(FrameError::BadId);
        }
        let sig: [u8; 64] = hex::decode(&self.sig)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or(FrameError::Malformed("sig"))?;
        let vk =
            VerifyingKey::from_bytes(&h.sender).map_err(|_| FrameError::Malformed("sender"))?;
        if !crate::keys::verify(&vk, &signing_bytes(&id), &Signature::from_bytes(&sig)) {
            return Err(FrameError::BadSignature);
        }
        Ok(h.sender)
    }

    /// Open a channel-sealed frame with a keyring wrapped to `me`. Does not
    /// verify; call [`verify`](Self::verify) first.
    pub fn open_channel(&self, keyring: &Keyring, me: &Identity) -> Result<Payload, FrameError> {
        let (h, body) = self.parsed()?;
        let Some(epoch) = h.epoch else {
            return Err(FrameError::WrongSealing);
        };
        let entry = keyring
            .entry(&epoch)
            .ok_or_else(|| FrameError::UnknownEpoch(hex::encode(epoch)))?;
        let key = keyring.key_for(entry, me)?;
        let plaintext = key.open(&Sealed::from_bytes(&body)?, &channel_aad(h.channel, &epoch))?;
        Ok(serde_json::from_slice(&plaintext)?)
    }

    /// Open a direct-sealed frame addressed to `me`. Does not verify; call
    /// [`verify`](Self::verify) first.
    pub fn open_direct(&self, me: &Identity) -> Result<Payload, FrameError> {
        let (h, body) = self.parsed()?;
        if h.epoch.is_some() {
            return Err(FrameError::WrongSealing);
        }
        let recipient = h.recipient.ok_or(FrameError::Malformed("recipient"))?;
        if recipient != me.verifying_key().to_bytes() {
            return Err(FrameError::NotRecipient);
        }
        let plaintext = me.open_sealed_box(
            &SealedBox::from_bytes(&body)?,
            &direct_aad(h.channel, &h.sender, &recipient),
        )?;
        Ok(serde_json::from_slice(&plaintext)?)
    }

    pub fn is_direct(&self) -> bool {
        matches!(self.sealing, Sealing::Direct)
    }
}

/// Inputs the spec-vector generator needs that are otherwise private:
/// `(channel_canon, channel_id, direct_canon, direct_id, signing_prefix)` for a
/// plaintext body. Not part of the wire API.
#[doc(hidden)]
#[allow(clippy::type_complexity)]
pub fn vector_support(
    sender: &Identity,
    recipient: &Identity,
    channel: &str,
    sent_ms: i64,
    payload: &Payload,
) -> (Vec<u8>, [u8; 32], Vec<u8>, [u8; 32], Vec<u8>) {
    let body = serde_json::to_vec(payload).unwrap();
    let s = sender.verifying_key().to_bytes();
    let ch = canonical_bytes(
        &Header {
            channel,
            sender: s,
            recipient: None,
            epoch: Some([0u8; EPOCH_ID_LEN]),
            sent_ms,
        },
        &body,
    );
    let dh = canonical_bytes(
        &Header {
            channel,
            sender: s,
            recipient: Some(recipient.verifying_key().to_bytes()),
            epoch: None,
            sent_ms,
        },
        &body,
    );
    let prefix = crate::version_label("frame").into_bytes();
    (ch.clone(), frame_id(&ch), dh.clone(), frame_id(&dh), prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::DataKey;

    fn ids() -> (Identity, Identity, Identity) {
        (
            Identity::from_seed(&[21u8; 32]),
            Identity::from_seed(&[22u8; 32]),
            Identity::from_seed(&[23u8; 32]),
        )
    }

    fn hello() -> Payload {
        Payload::Hello {
            node: serde_json::json!({"name": "bazzite"}),
            contract: crate::CONTRACT_VERSION,
        }
    }

    #[test]
    fn channel_frame_round_trip_and_tamper() {
        let (a, b, c) = ids();
        let key = DataKey::generate();
        let ring_a = Keyring::genesis(&a.x25519_public(), &key);
        let ring_b = ring_a.wrap_for(&a, &b.x25519_public()).unwrap();

        let f = Envelope::seal_channel(&a, "personal", None, &ring_a, &hello(), 1).unwrap();
        // Serde round trip, then verify and open as B.
        let json = serde_json::to_string(&f).unwrap();
        let f: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(f.verify().unwrap(), a.verifying_key().to_bytes());
        assert_eq!(f.open_channel(&ring_b, &b).unwrap(), hello());
        // C has no keyring for the channel.
        let ring_c = Keyring::genesis(&c.x25519_public(), &DataKey::generate());
        assert!(matches!(
            f.open_channel(&ring_c, &c),
            Err(FrameError::Envelope(_))
        ));

        // Re-labelled channel: id no longer matches.
        let mut t = f.clone();
        t.channel = "work".into();
        assert!(matches!(t.verify(), Err(FrameError::BadId)));
        // Body swapped: id mismatch.
        let mut t = f.clone();
        t.body = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"xx");
        assert!(matches!(t.verify(), Err(FrameError::BadId)));
        // Signature from another key over the same id.
        let mut t = f.clone();
        let id = hex::decode(&f.id).unwrap().try_into().unwrap();
        t.sig = hex::encode(b.sign(&signing_bytes(&id)).to_bytes());
        assert!(matches!(t.verify(), Err(FrameError::BadSignature)));
        // Unknown epoch.
        let mut t = f.clone();
        t.sealing = Sealing::Channel {
            epoch: hex::encode([9u8; 16]),
        };
        assert!(matches!(
            t.open_channel(&ring_b, &b),
            Err(FrameError::UnknownEpoch(_))
        ));
        // Wrong open path.
        assert!(matches!(f.open_direct(&b), Err(FrameError::WrongSealing)));
    }

    #[test]
    fn direct_frame_round_trip() {
        let (a, b, c) = ids();
        let p = Payload::Command {
            cmd_id: "x".into(),
            command: Command::Kill {
                session_id: "s1".into(),
            },
        };
        let f = Envelope::seal_direct(&a, MESH_CHANNEL, &b.node_id(), &b.x25519_public(), &p, 2)
            .unwrap();
        assert!(f.is_direct());
        f.verify().unwrap();
        assert_eq!(f.open_direct(&b).unwrap(), p);
        assert!(matches!(f.open_direct(&c), Err(FrameError::NotRecipient)));
        // Channel open on a direct frame is refused before any key use.
        let ring = Keyring::genesis(&b.x25519_public(), &DataKey::generate());
        assert!(matches!(
            f.open_channel(&ring, &b),
            Err(FrameError::WrongSealing)
        ));
        // Recipient swap fails the id.
        let mut t = f.clone();
        t.recipient = Some(c.node_id());
        assert!(matches!(t.verify(), Err(FrameError::BadId)));
    }

    #[test]
    fn channel_names() {
        assert!(valid_channel("@mesh"));
        assert!(valid_channel("client-hdr.2026"));
        assert!(!valid_channel(""));
        assert!(!valid_channel("Work"));
        assert!(!valid_channel("a b"));
        assert!(!valid_channel(&"x".repeat(65)));
    }

    #[test]
    fn payload_wire_names_are_stable() {
        let v = serde_json::to_value(Payload::EventsRequest {
            session_id: "s".into(),
            after_origin_seq: 3,
        })
        .unwrap();
        assert_eq!(v["kind"], "events_request");
        let v = serde_json::to_value(Payload::CredentialHandoff {
            credentials: vec![],
        })
        .unwrap();
        assert_eq!(v["kind"], "credential_handoff");
        let v = serde_json::to_value(Command::Prompt {
            session_id: "s".into(),
            text: "t".into(),
        })
        .unwrap();
        assert_eq!(v["op"], "prompt");
    }

    #[derive(serde::Deserialize)]
    struct VectorFile {
        contract_version: u32,
        sender_seed_hex: String,
        recipient_seed_hex: String,
        channel: String,
        sent_ms: i64,
        payload: Value,
        channel_key_hex: String,
        channel_epoch_hex: String,
        channel_canon_hex: String,
        channel_id_hex: String,
        direct_canon_hex: String,
        direct_id_hex: String,
        signing_prefix_hex: String,
    }
    const VECTORS: &str = include_str!("../../spec/vectors/frame.json");

    /// The canonical bytes and id are pinned for a fixed body; sealing itself is
    /// randomized, so the vector fixes the sealed body bytes directly.
    #[test]
    fn matches_spec_vectors() {
        let f: VectorFile = serde_json::from_str(VECTORS).unwrap();
        assert_eq!(f.contract_version, crate::CONTRACT_VERSION);
        let s = Identity::from_seed(&key32(&f.sender_seed_hex).unwrap());
        let r = Identity::from_seed(&key32(&f.recipient_seed_hex).unwrap());
        let payload: Payload = serde_json::from_value(f.payload.clone()).unwrap();
        let body = serde_json::to_vec(&payload).unwrap();
        let epoch: [u8; 16] = hex::decode(&f.channel_epoch_hex)
            .unwrap()
            .try_into()
            .unwrap();
        // Channel header: no recipient, epoch set. Body here is the *plaintext*
        // (vectors pin the canonical encoding, not a random seal).
        let h = Header {
            channel: &f.channel,
            sender: s.verifying_key().to_bytes(),
            recipient: None,
            epoch: Some(epoch),
            sent_ms: f.sent_ms,
        };
        let canon = canonical_bytes(&h, &body);
        assert_eq!(hex::encode(&canon), f.channel_canon_hex);
        assert_eq!(hex::encode(frame_id(&canon)), f.channel_id_hex);
        let h = Header {
            channel: &f.channel,
            sender: s.verifying_key().to_bytes(),
            recipient: Some(r.verifying_key().to_bytes()),
            epoch: None,
            sent_ms: f.sent_ms,
        };
        let canon = canonical_bytes(&h, &body);
        assert_eq!(hex::encode(&canon), f.direct_canon_hex);
        assert_eq!(hex::encode(frame_id(&canon)), f.direct_id_hex);
        assert_eq!(
            hex::encode(&signing_bytes(&[0u8; 32])[..crate::version_label("frame").len()]),
            f.signing_prefix_hex
        );
        // And the channel key from the vector opens a frame sealed under it.
        let key = DataKey::from_bytes(key32(&f.channel_key_hex).unwrap());
        let ring = Keyring::genesis(&s.x25519_public(), &key);
        let env = Envelope::seal_channel(&s, &f.channel, None, &ring, &payload, f.sent_ms).unwrap();
        env.verify().unwrap();
    }
}
