//! Owner streams: a bounded, encrypted, **relayed** frame family, distinct
//! from the durable [`crate::frame`] path.
//!
//! A durable frame is stored by the hub, pulled by cursor, deduplicated by id
//! and replayed until it is applied. That is exactly wrong for a request whose
//! answer is only interesting now: replaying a `POST` after a reconnect would
//! repeat a mutation, and a session's event stream would fill the hub's disk
//! with bytes nobody will ever read again. So a stream is its own family:
//!
//! - **Relayed, never stored.** The hub holds a bounded in-memory buffer per
//!   stream and forgets everything the moment it is delivered or dropped.
//! - **Its own key.** The per-stream key is derived from the channel epoch key
//!   with a distinct HKDF label, so a stream frame can never be opened as a
//!   durable one and the nonce spaces cannot overlap: durable frames carry
//!   random nonces under the epoch key, stream frames carry a structured
//!   counter nonce under a key that epoch key never seals anything with.
//! - **Its own replay rule.** There is no frame-id dedupe table. `(stream_id,
//!   sender, seq)` is strictly increasing per direction; a repeat or a
//!   regression is dropped, and the counter nonce makes a repeated `seq` a
//!   decryption failure rather than a policy question.
//!
//! ```text
//! { v, channel, sender, recipient, stream_id, epoch, seq, sent_ms, body, sig }
//! ```
//!
//! Everything but `body` is routing metadata the hub reads; `body` is the
//! sealed [`StreamFrame`]. Sealed then signed, exactly as durable frames are:
//! the hub and the peer verify authorship before any decrypt path runs.
//!
//! The AAD binds every clear field, so a frame the hub re-labels onto another
//! stream, another channel, another epoch, another recipient or another
//! sequence number fails to open.

use serde::{Deserialize, Serialize};

use ed25519_dalek::{Signature, VerifyingKey};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

use crate::envelope::{DataKey, EnvelopeError, Sealed, NONCE_LEN};
use crate::keyring::{Keyring, AAD_SEP, EPOCH_ID_LEN};
use crate::keys::{key32, Identity};
use crate::{put_bytes, put_str};

/// The stream protocol's own version, carried inside [`StreamOpen`] and
/// checked by the owner. It moves independently of
/// [`CONTRACT_VERSION`](crate::CONTRACT_VERSION): a peer that does not know
/// streams at all refuses the envelope by name (the route 404s, or the frame
/// fails to deserialize), and a peer that knows a *different* stream protocol
/// answers [`StreamFrame::Refused`] with [`refused::VERSION`].
pub const STREAM_PROTOCOL_VERSION: u32 = 1;

/// A stream id is 16 random bytes, hex.
pub const STREAM_ID_LEN: usize = 16;

/// Hard cap on one serialized stream envelope. Smaller than a durable frame
/// on purpose: a stream is a pipe, and a pipe made of 4 MiB frames defeats
/// the credit window it is flow-controlled by.
pub const MAX_STREAM_FRAME_BYTES: usize = 1024 * 1024;

/// Hard cap on the plaintext bytes one [`StreamFrame::Chunk`] may carry. The
/// serialized envelope is base64 of the sealed form of this, so the two caps
/// are consistent with room to spare for headers.
pub const MAX_CHUNK_BYTES: usize = 512 * 1024;

/// The most request headers an open may name. The allowlist is what bounds
/// the *content*; this bounds the count.
pub const MAX_OPEN_HEADERS: usize = 32;

const HKDF_LABEL: &str = "stream-key";
const NONCE_TAG: &[u8; 4] = b"trst";
const SIG_LABEL: &str = "stream";

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("malformed stream frame field: {0}")]
    Malformed(&'static str),
    #[error("stream frame wire version {got} is not this build's contract version {expected}")]
    UnsupportedVersion { got: u32, expected: u32 },
    #[error("stream frame signature does not verify")]
    BadSignature,
    #[error("stream frame is not addressed to this node")]
    NotRecipient,
    #[error("no key for epoch {0}")]
    UnknownEpoch(String),
    #[error("stream frame exceeds the {MAX_STREAM_FRAME_BYTES} byte limit")]
    TooLarge,
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    #[error("stream payload is not valid JSON: {0}")]
    Payload(#[from] serde_json::Error),
}

/// Which end of a stream sealed a frame. The two directions derive different
/// keys from the same stream id, so both may count from `seq = 0` without
/// ever repeating a `(key, nonce)` pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// The node that opened the stream: the one serving the operator.
    Serving,
    /// The node that owns the session.
    Owner,
}

impl Direction {
    fn label(self) -> &'static str {
        match self {
            Direction::Serving => "serving",
            Direction::Owner => "owner",
        }
    }

    /// The direction a peer's frames arrive in, given what this node is.
    pub fn peer(self) -> Direction {
        match self {
            Direction::Serving => Direction::Owner,
            Direction::Owner => Direction::Serving,
        }
    }
}

/// What kind of thing the stream carries. The owner uses it to decide how the
/// local response is read back: buffered, or streamed until close.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    /// One request, one response. Bounded by size and by timeout.
    Http,
    /// A long-lived server-sent event stream, chunked until close.
    Sse,
    /// A duplex WebSocket (the PTY). Input is never replayed after a
    /// reconnect; a dropped socket is a new stream or nothing.
    Ws,
}

/// The request metadata an open may carry. Deliberately an allowlist: the
/// owner repeats its own authorization from these fields and its own state,
/// never from anything the serving node asserts about the operator's rights.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamOpen {
    /// The stream protocol the opener speaks.
    pub protocol_version: u32,
    /// The session on the owner this stream is for.
    pub session_id: String,
    pub kind: StreamKind,
    /// Uppercase HTTP method.
    pub method: String,
    /// The gateway-relative path, already normalised by the serving node. The
    /// owner normalises it again; this one is a hint, not a warrant.
    pub path: String,
    /// The raw query string, without the leading `?`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// The allowlisted request headers, lowercase names, in order.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// `sha256` hex of the whole request body, which travels as chunks. The
    /// owner refuses a body whose digest does not match, so a truncated or
    /// re-ordered upload is never dispatched as a mutation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_sha256: Option<String>,
    /// How many bytes the body is, so the owner can refuse before reading.
    #[serde(default)]
    pub body_len: u64,
    /// Who is asking, as the serving node knows them. Evidence for the
    /// owner's record, never a grant: the owner decides from its own
    /// membership, policy and gateway matrix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    /// The owner epoch the serving node believes it is still talking to, from
    /// a previous stream's [`StreamFrame::Head`]. A mismatch is fenced by the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_epoch: Option<String>,
}

/// The reasons an open is refused, as machine-readable codes. The operator
/// reads the prose in `detail`; the serving node branches on these.
pub mod refused {
    /// The opener speaks a stream protocol this owner does not.
    pub const VERSION: &str = "unsupported_version";
    /// No such session here, or it is not this node's to serve.
    pub const UNKNOWN_SESSION: &str = "unknown_session";
    /// The opener is not (or is no longer) a member of the session's channel.
    pub const REVOKED_MEMBER: &str = "revoked_member";
    /// The opener named an owner epoch this node has moved past.
    pub const FENCED: &str = "owner_fenced";
    /// The gateway, the policy, or the session state said no.
    pub const REFUSED: &str = "refused";
    /// Too many streams already open for this peer.
    pub const BUSY: &str = "busy";
    /// The request metadata does not describe a request this owner will run.
    pub const MALFORMED: &str = "malformed";
}

/// Why a stream closed. `detail` carries the operator-readable sentence.
pub mod closed {
    pub const DONE: &str = "done";
    pub const TIMEOUT: &str = "timeout";
    pub const TOO_LARGE: &str = "too_large";
    /// The hub dropped it: the buffer filled, or a limit was reached.
    pub const RELAY_DROPPED: &str = "relay_dropped";
    /// The owner's session epoch moved: a restart, or the session ended.
    pub const FENCED: &str = "owner_fenced";
    /// Membership or a key was revoked while the stream was open.
    pub const REVOKED: &str = "revoked";
    pub const PEER_GONE: &str = "peer_gone";
    pub const LOCAL_ERROR: &str = "local_error";
}

/// The sealed plaintext of one stream envelope.
///
/// `serde` is tagged and every added field is defaulted, so a peer on an
/// older build refuses an unknown variant **by name** — the whole payload
/// fails to deserialize, the receiver answers [`StreamFrame::Refused`] with
/// [`refused::VERSION`], and no contract version needs to move (#179's rule).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum StreamFrame {
    /// Serving → owner. Begins the stream.
    Open(Box<StreamOpen>),
    /// Owner → serving. The response head, once the owner has authorized and
    /// dispatched. `owner_epoch` is what the serving node fences on next time.
    Head {
        status: u16,
        #[serde(default)]
        headers: Vec<(String, String)>,
        owner_epoch: String,
        /// True when the owner could not learn whether a mutation happened.
        /// The serving node reports it; it never retries on its own.
        #[serde(default)]
        uncertain: bool,
    },
    /// Either direction. `bytes` is base64 (standard, padded). `fin` ends
    /// this direction; the other may still be sending.
    Chunk {
        #[serde(default)]
        bytes: String,
        #[serde(default)]
        fin: bool,
    },
    /// Either direction. Grants the peer `credit` more chunks. A sender with
    /// no credit stops; it never drops a chunk to keep going.
    Flow { credit: u32 },
    /// Either direction. Tears the stream down, saying why.
    Close {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Owner → serving, in place of a head. The stream is over.
    Refused {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

impl StreamFrame {
    /// A short name for logs and refusal messages.
    pub fn name(&self) -> &'static str {
        match self {
            StreamFrame::Open(_) => "open",
            StreamFrame::Head { .. } => "head",
            StreamFrame::Chunk { .. } => "chunk",
            StreamFrame::Flow { .. } => "flow",
            StreamFrame::Close { .. } => "close",
            StreamFrame::Refused { .. } => "refused",
        }
    }

    /// The chunk's plaintext bytes, decoded.
    pub fn chunk_bytes(&self) -> Option<Vec<u8>> {
        use base64::Engine;
        match self {
            StreamFrame::Chunk { bytes, .. } => {
                base64::engine::general_purpose::STANDARD.decode(bytes).ok()
            }
            _ => None,
        }
    }

    pub fn chunk(bytes: &[u8], fin: bool) -> StreamFrame {
        use base64::Engine;
        StreamFrame::Chunk {
            bytes: base64::engine::general_purpose::STANDARD.encode(bytes),
            fin,
        }
    }
}

/// One relayed stream envelope. Everything but `body` is what the hub routes
/// on; `body` is opaque to it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamEnvelope {
    pub v: u32,
    pub channel: String,
    pub sender: String,
    pub recipient: String,
    /// Hex of [`STREAM_ID_LEN`] bytes.
    pub stream_id: String,
    /// Hex of the channel epoch the per-stream key is derived from.
    pub epoch: String,
    /// Strictly increasing per `(stream_id, sender)`, from 0.
    pub seq: u64,
    pub sent_ms: i64,
    /// base64 (standard, padded) of the sealed bytes.
    pub body: String,
    pub sig: String,
}

fn stream_id32(hex: &str) -> Option<[u8; STREAM_ID_LEN]> {
    hex::decode(hex).ok().and_then(|b| b.try_into().ok())
}

fn epoch32(hex: &str) -> Option<[u8; EPOCH_ID_LEN]> {
    hex::decode(hex).ok().and_then(|b| b.try_into().ok())
}

/// Mint a stream id.
pub fn new_stream_id() -> String {
    use rand_core::{OsRng, RngCore};
    let mut id = [0u8; STREAM_ID_LEN];
    OsRng.fill_bytes(&mut id);
    hex::encode(id)
}

/// The per-stream, per-direction key.
///
/// ```text
/// salt = stream_id(16) ‖ 0x1f ‖ sender(32) ‖ recipient(32)
/// info = "tracon/v0/stream-key" ‖ 0x1f ‖ channel ‖ 0x1f ‖ epoch(16) ‖ 0x1f ‖ direction
/// ```
///
/// The label differs from every durable label, so the epoch key that seals
/// frames cannot seal or open a stream frame, and a stream key is useless on
/// the durable path. Binding both node ids means a third member holding the
/// same epoch key derives a different key and opens nothing.
fn stream_key(
    epoch_key: &DataKey,
    channel: &str,
    epoch: &[u8; EPOCH_ID_LEN],
    stream_id: &[u8; STREAM_ID_LEN],
    sender: &[u8; 32],
    recipient: &[u8; 32],
    direction: Direction,
) -> DataKey {
    let mut salt = Vec::with_capacity(STREAM_ID_LEN + 1 + 64);
    salt.extend_from_slice(stream_id);
    salt.push(AAD_SEP);
    salt.extend_from_slice(sender);
    salt.extend_from_slice(recipient);

    let mut info = crate::version_label(HKDF_LABEL).into_bytes();
    info.push(AAD_SEP);
    info.extend_from_slice(channel.as_bytes());
    info.push(AAD_SEP);
    info.extend_from_slice(epoch);
    info.push(AAD_SEP);
    info.extend_from_slice(direction.label().as_bytes());

    let raw = epoch_key.to_bytes();
    let hk = Hkdf::<Sha256>::new(Some(&salt), &raw);
    let mut okm = [0u8; 32];
    hk.expand(&info, &mut okm)
        .expect("HKDF expand of 32 bytes is always within bounds");
    DataKey::from_bytes(okm)
}

/// The counter nonce. `"trst" ‖ u64_be(seq) ‖ direction(1) ‖ zeros`, which is
/// not a value the durable path can produce: durable nonces are random under
/// a different key entirely, and this one is deterministic. Reusing a `seq`
/// therefore reuses a `(key, nonce)` pair, which the receiver's monotonicity
/// check refuses before the AEAD ever sees it.
fn stream_nonce(seq: u64, direction: Direction) -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    nonce[..4].copy_from_slice(NONCE_TAG);
    nonce[4..12].copy_from_slice(&seq.to_be_bytes());
    nonce[12] = match direction {
        Direction::Serving => 1,
        Direction::Owner => 2,
    };
    nonce
}

/// Everything clear in the envelope, in canonical form. Both the AAD and the
/// signing bytes are built from this, so a re-labelled frame fails twice.
fn canonical(
    channel: &str,
    sender: &[u8; 32],
    recipient: &[u8; 32],
    stream_id: &[u8; STREAM_ID_LEN],
    epoch: &[u8; EPOCH_ID_LEN],
    seq: u64,
    sent_ms: i64,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&crate::CONTRACT_VERSION.to_be_bytes());
    put_str(&mut out, channel);
    out.extend_from_slice(sender);
    out.extend_from_slice(recipient);
    out.extend_from_slice(stream_id);
    out.extend_from_slice(epoch);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&sent_ms.to_be_bytes());
    out
}

/// `"tracon/v0/stream" ‖ SHA256(canonical ‖ len+body)`. The body is the
/// sealed bytes, so the signature covers the ciphertext and every clear field
/// at once: sealed, then signed, exactly as a durable frame is.
fn signing_bytes(canon: &[u8], body: &[u8]) -> Vec<u8> {
    let mut covered = canon.to_vec();
    put_bytes(&mut covered, body);
    let mut h = Sha256::new();
    h.update(&covered);
    let digest: [u8; 32] = h.finalize().into();
    let mut out = crate::version_label(SIG_LABEL).into_bytes();
    out.extend_from_slice(&digest);
    out
}

impl StreamEnvelope {
    /// Seal and sign one stream frame.
    #[allow(clippy::too_many_arguments)]
    pub fn seal(
        sender: &Identity,
        channel: &str,
        recipient_node_id: &str,
        keyring: &Keyring,
        epoch_hex: &str,
        stream_id_hex: &str,
        seq: u64,
        direction: Direction,
        frame: &StreamFrame,
        sent_ms: i64,
    ) -> Result<StreamEnvelope, StreamError> {
        use base64::Engine;
        let s = sender.verifying_key().to_bytes();
        let r = key32(recipient_node_id).ok_or(StreamError::Malformed("recipient"))?;
        let sid = stream_id32(stream_id_hex).ok_or(StreamError::Malformed("stream_id"))?;
        let epoch = epoch32(epoch_hex).ok_or(StreamError::Malformed("epoch"))?;
        let entry = keyring
            .entry(&epoch)
            .ok_or_else(|| StreamError::UnknownEpoch(epoch_hex.to_string()))?;
        let epoch_key = keyring.key_for(entry, sender)?;
        let key = stream_key(&epoch_key, channel, &epoch, &sid, &s, &r, direction);
        let canon = canonical(channel, &s, &r, &sid, &epoch, seq, sent_ms);
        let plaintext = serde_json::to_vec(frame)?;
        let body = key
            .seal_with_nonce(stream_nonce(seq, direction), &plaintext, &canon)
            .to_bytes();
        let sig = sender.sign(&signing_bytes(&canon, &body));
        Ok(StreamEnvelope {
            v: crate::CONTRACT_VERSION,
            channel: channel.to_string(),
            sender: hex::encode(s),
            recipient: hex::encode(r),
            stream_id: stream_id_hex.to_ascii_lowercase(),
            epoch: hex::encode(epoch),
            seq,
            sent_ms,
            body: base64::engine::general_purpose::STANDARD.encode(body),
            sig: hex::encode(sig.to_bytes()),
        })
    }

    fn parts(
        &self,
    ) -> Result<
        (
            [u8; 32],
            [u8; 32],
            [u8; STREAM_ID_LEN],
            [u8; EPOCH_ID_LEN],
            Vec<u8>,
        ),
        StreamError,
    > {
        use base64::Engine;
        if !crate::frame::valid_channel(&self.channel) {
            return Err(StreamError::Malformed("channel"));
        }
        let sender = key32(&self.sender).ok_or(StreamError::Malformed("sender"))?;
        let recipient = key32(&self.recipient).ok_or(StreamError::Malformed("recipient"))?;
        let sid = stream_id32(&self.stream_id).ok_or(StreamError::Malformed("stream_id"))?;
        let epoch = epoch32(&self.epoch).ok_or(StreamError::Malformed("epoch"))?;
        let body = base64::engine::general_purpose::STANDARD
            .decode(&self.body)
            .map_err(|_| StreamError::Malformed("body"))?;
        Ok((sender, recipient, sid, epoch, body))
    }

    /// Check the wire version and the signature. Returns the sender's key.
    /// The hub calls this before it relays anything, so a frame it cannot
    /// attribute never reaches a buffer.
    pub fn verify(&self) -> Result<[u8; 32], StreamError> {
        if self.v != crate::CONTRACT_VERSION {
            return Err(StreamError::UnsupportedVersion {
                got: self.v,
                expected: crate::CONTRACT_VERSION,
            });
        }
        let (sender, recipient, sid, epoch, body) = self.parts()?;
        let canon = canonical(
            &self.channel,
            &sender,
            &recipient,
            &sid,
            &epoch,
            self.seq,
            self.sent_ms,
        );
        let sig: [u8; 64] = hex::decode(&self.sig)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or(StreamError::Malformed("sig"))?;
        let vk = VerifyingKey::from_bytes(&sender).map_err(|_| StreamError::Malformed("sender"))?;
        if !crate::keys::verify(
            &vk,
            &signing_bytes(&canon, &body),
            &Signature::from_bytes(&sig),
        ) {
            return Err(StreamError::BadSignature);
        }
        Ok(sender)
    }

    /// Open a frame addressed to `me`. `direction` is the direction the
    /// *sender* sealed in. Does not verify; call [`verify`](Self::verify).
    pub fn open(
        &self,
        keyring: &Keyring,
        me: &Identity,
        direction: Direction,
    ) -> Result<StreamFrame, StreamError> {
        let (sender, recipient, sid, epoch, body) = self.parts()?;
        if recipient != me.verifying_key().to_bytes() {
            return Err(StreamError::NotRecipient);
        }
        let entry = keyring
            .entry(&epoch)
            .ok_or_else(|| StreamError::UnknownEpoch(hex::encode(epoch)))?;
        let epoch_key = keyring.key_for(entry, me)?;
        let key = stream_key(
            &epoch_key,
            &self.channel,
            &epoch,
            &sid,
            &sender,
            &recipient,
            direction,
        );
        let canon = canonical(
            &self.channel,
            &sender,
            &recipient,
            &sid,
            &epoch,
            self.seq,
            self.sent_ms,
        );
        let plaintext = key.open(&Sealed::from_bytes(&body)?, &canon)?;
        Ok(serde_json::from_slice(&plaintext)?)
    }
}

/// The per-`(stream_id, sender)` monotonicity check. There is no dedupe table
/// and no window: a stream's frames arrive in order on one relay connection,
/// and anything else is a replay.
#[derive(Debug, Default)]
pub struct SeqGuard {
    next: std::collections::HashMap<String, u64>,
}

impl SeqGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` if `seq` is the next one expected from `key`. A repeat, a gap
    /// backwards, or a jump forwards is refused: a stream with a hole in it
    /// is not a stream, it is a truncated one pretending otherwise.
    pub fn admit(&mut self, key: &str, seq: u64) -> bool {
        let slot = self.next.entry(key.to_string()).or_insert(0);
        if seq != *slot {
            return false;
        }
        *slot += 1;
        true
    }

    pub fn forget(&mut self, key: &str) {
        self.next.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyring::Keyring;

    fn pair() -> (Identity, Identity, Keyring, Keyring) {
        let a = Identity::from_seed(&[31u8; 32]);
        let b = Identity::from_seed(&[32u8; 32]);
        let ring_a = Keyring::genesis(&a.x25519_public(), &DataKey::generate());
        let ring_b = ring_a.wrap_for(&a, &b.x25519_public()).unwrap();
        (a, b, ring_a, ring_b)
    }

    fn open_frame() -> StreamFrame {
        StreamFrame::Open(Box::new(StreamOpen {
            protocol_version: STREAM_PROTOCOL_VERSION,
            session_id: "ses_1".into(),
            kind: StreamKind::Http,
            method: "GET".into(),
            path: "api/session/ses_x/message".into(),
            query: Some("after=7".into()),
            headers: vec![("accept".into(), "application/json".into())],
            body_sha256: None,
            body_len: 0,
            operator: Some("op".into()),
            owner_epoch: None,
        }))
    }

    fn epoch_hex(ring: &Keyring) -> String {
        hex::encode(ring.newest().id())
    }

    #[test]
    fn a_stream_frame_round_trips_and_tamper_fails() {
        let (a, b, ring_a, ring_b) = pair();
        let sid = new_stream_id();
        let e = epoch_hex(&ring_a);
        let env = StreamEnvelope::seal(
            &a,
            "personal",
            &b.node_id(),
            &ring_a,
            &e,
            &sid,
            0,
            Direction::Serving,
            &open_frame(),
            7,
        )
        .unwrap();
        let json = serde_json::to_string(&env).unwrap();
        let env: StreamEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(env.verify().unwrap(), a.verifying_key().to_bytes());
        assert_eq!(
            env.open(&ring_b, &b, Direction::Serving).unwrap(),
            open_frame()
        );

        // The hub re-labels the channel: signature fails.
        let mut moved = env.clone();
        moved.channel = "work".into();
        assert!(matches!(moved.verify(), Err(StreamError::BadSignature)));
        // The hub re-labels the stream id: signature fails.
        let mut other = env.clone();
        other.stream_id = new_stream_id();
        assert!(matches!(other.verify(), Err(StreamError::BadSignature)));
        // A frame claiming the wrong direction does not open.
        assert!(env.open(&ring_b, &b, Direction::Owner).is_err());
        // A frame for someone else is not opened here.
        let c = Identity::from_seed(&[33u8; 32]);
        assert!(matches!(
            env.open(&ring_b, &c, Direction::Serving),
            Err(StreamError::NotRecipient)
        ));
    }

    /// The two directions must never derive the same key, or both counting
    /// from zero would repeat a `(key, nonce)` pair.
    #[test]
    fn the_two_directions_are_different_keys() {
        let (a, b, ring_a, ring_b) = pair();
        let sid = new_stream_id();
        let e = epoch_hex(&ring_a);
        let frame = StreamFrame::chunk(b"hello", false);
        let env = StreamEnvelope::seal(
            &a,
            "personal",
            &b.node_id(),
            &ring_a,
            &e,
            &sid,
            0,
            Direction::Serving,
            &frame,
            1,
        )
        .unwrap();
        let other = StreamEnvelope::seal(
            &a,
            "personal",
            &b.node_id(),
            &ring_a,
            &e,
            &sid,
            0,
            Direction::Owner,
            &frame,
            1,
        )
        .unwrap();
        assert_ne!(env.body, other.body);
        assert_eq!(
            env.open(&ring_b, &b, Direction::Serving)
                .unwrap()
                .chunk_bytes()
                .unwrap(),
            b"hello"
        );
    }

    /// A durable frame's body must not open as a stream body, and the stream
    /// key must not be the epoch key.
    #[test]
    fn the_stream_key_is_not_the_epoch_key() {
        let (a, b, ring_a, ring_b) = pair();
        let sid = new_stream_id();
        let e = epoch_hex(&ring_a);
        let env = StreamEnvelope::seal(
            &a,
            "personal",
            &b.node_id(),
            &ring_a,
            &e,
            &sid,
            0,
            Direction::Serving,
            &StreamFrame::chunk(b"x", true),
            1,
        )
        .unwrap();
        use base64::Engine;
        let body = base64::engine::general_purpose::STANDARD
            .decode(&env.body)
            .unwrap();
        let sealed = Sealed::from_bytes(&body).unwrap();
        let epoch_key = ring_b.key_for(ring_b.newest(), &b).unwrap();
        // The channel epoch key opens nothing here, under any of the AADs the
        // durable path uses.
        assert!(epoch_key
            .open(
                &sealed,
                &crate::keyring::channel_aad("personal", &[0u8; 16])
            )
            .is_err());
        assert!(epoch_key.open(&sealed, &[]).is_err());
        let _ = ring_a;
    }

    #[test]
    fn a_sequence_is_admitted_once_and_in_order() {
        let mut g = SeqGuard::new();
        assert!(g.admit("s", 0));
        assert!(!g.admit("s", 0), "a replay is refused");
        assert!(!g.admit("s", 2), "a gap is refused");
        assert!(g.admit("s", 1));
        g.forget("s");
        assert!(g.admit("s", 0));
    }

    /// #179's rule: an unknown variant fails the whole payload, so a peer on
    /// an older build refuses by name rather than half-reading a frame.
    #[test]
    fn an_unknown_variant_fails_the_payload() {
        let raw = serde_json::json!({ "frame": "teleport", "somewhere": 1 });
        assert!(serde_json::from_value::<StreamFrame>(raw).is_err());
        // But an added, defaulted field on a known variant is read.
        let raw = serde_json::json!({ "frame": "chunk", "bytes": "", "fin": true, "later": 2 });
        assert_eq!(
            serde_json::from_value::<StreamFrame>(raw).unwrap(),
            StreamFrame::Chunk {
                bytes: String::new(),
                fin: true
            }
        );
    }
}
