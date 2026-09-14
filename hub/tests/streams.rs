//! The relay half: what the hub does with an owner stream, and what it can
//! never do with one.
//!
//! The load-bearing claim is negative — the hub routes and bounds, and holds
//! no key — so these tests assert on the bytes it relayed and on the frame
//! store it did *not* write to, rather than on its own account of itself.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use hub::store::{FrameStore, Member, MemberStore, MemoryFrames, MemoryMembers};
use hub::streams::{StreamLimits, StreamRelay};
use hub::{AppState, HubConfig};
use proto::auth::signed_headers;
use proto::envelope::DataKey;
use proto::keyring::Keyring;
use proto::keys::Identity;
use proto::stream::{
    new_stream_id, Direction, StreamEnvelope, StreamFrame, StreamKind, StreamOpen,
    STREAM_PROTOCOL_VERSION,
};
use serde_json::Value;
use tower::ServiceExt;

#[path = "support/mod.rs"]
mod support;
use support::{now, now_ms};

const CHANNEL: &str = "personal";
/// A string that exists only inside the sealed body. If the hub ever holds
/// plaintext, this is what shows up in what it relayed.
const SECRET: &str = "a-path-only-the-two-nodes-know";

struct Rig {
    app: Router,
    state: AppState,
    frames: Arc<MemoryFrames>,
    a: Identity,
    b: Identity,
    ring_a: Keyring,
    ring_b: Keyring,
}

fn build(limits: StreamLimits, channels_b: &[&str]) -> Rig {
    let a = Identity::from_seed(&[11u8; 32]);
    let b = Identity::from_seed(&[12u8; 32]);
    let members = Arc::new(MemoryMembers::new());
    for (id, channels) in [(&a, &[CHANNEL][..]), (&b, channels_b)] {
        members
            .put(&Member {
                node_id: id.node_id(),
                x25519_pub: id.x25519_hex(),
                binding_sig: proto::enroll::sign_binding(id),
                name: "n".into(),
                channels: channels.iter().map(|s| s.to_string()).collect(),
                admitted_ms: 0,
                admitted_by: "test".into(),
                role: Default::default(),
            })
            .unwrap();
    }
    let frames = Arc::new(MemoryFrames::new());
    let mut state = hub::state_for(
        frames.clone(),
        members.clone(),
        HubConfig::default(),
        Arc::new(hub::pokes::PokeHub::new()),
        None,
    );
    state.streams = Arc::new(StreamRelay::new(limits));
    let app = hub::app_with_state(state.clone());
    let ring_a = Keyring::genesis(&a.x25519_public(), &DataKey::generate());
    let ring_b = ring_a.wrap_for(&a, &b.x25519_public()).unwrap();
    Rig {
        app,
        state,
        frames,
        a,
        b,
        ring_a,
        ring_b,
    }
}

impl Rig {
    fn open_frame(&self, stream_id: &str, seq: u64) -> StreamEnvelope {
        self.frame(
            stream_id,
            seq,
            &StreamFrame::Open(Box::new(StreamOpen {
                protocol_version: STREAM_PROTOCOL_VERSION,
                session_id: "s-1".into(),
                kind: StreamKind::Http,
                method: "GET".into(),
                path: SECRET.into(),
                query: None,
                headers: Vec::new(),
                body_sha256: None,
                body_len: 0,
                operator: None,
                owner_epoch: None,
            })),
        )
    }

    fn frame(&self, stream_id: &str, seq: u64, frame: &StreamFrame) -> StreamEnvelope {
        StreamEnvelope::seal(
            &self.a,
            CHANNEL,
            &self.b.node_id(),
            &self.ring_a,
            &hex::encode(self.ring_a.newest().id()),
            stream_id,
            seq,
            Direction::Serving,
            frame,
            now_ms(),
        )
        .unwrap()
    }

    async fn post(&self, id: &Identity, env: &StreamEnvelope) -> (StatusCode, Value) {
        let body = serde_json::to_string(env).unwrap();
        self.send(id, "POST", "/v0/streams", &body).await
    }

    async fn send(
        &self,
        id: &Identity,
        method: &str,
        path: &str,
        body: &str,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in signed_headers(id, method, path, body.as_bytes(), now()) {
            b = b.header(k, v);
        }
        let req = b
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Everything the hub still holds on any channel, as one string.
    fn durable(&self) -> String {
        let mut out = String::new();
        for channel in self.frames.channels().unwrap() {
            for (_, frame) in self.frames.read(&channel, 0, 1000).unwrap().frames {
                out.push_str(&frame);
            }
        }
        out
    }
}

/// The hub relays the exact bytes it was given, holds no key, and writes
/// nothing down.
#[tokio::test]
async fn a_relayed_frame_is_ciphertext_and_routing_metadata_only() {
    let rig = build(StreamLimits::default(), &[CHANNEL]);
    let mut rx = rig.state.streams.connect(&rig.b.node_id());
    let stream_id = new_stream_id();
    let env = rig.open_frame(&stream_id, 0);
    let (status, _) = rig.post(&rig.a, &env).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let delivered = rx.recv().await.unwrap();
    assert_eq!(delivered.event, "stream");
    // The bytes are unchanged, so the far side's signature check still covers
    // what it reads.
    let back: Value = serde_json::from_str(&delivered.data).unwrap();
    assert_eq!(back, serde_json::to_value(&env).unwrap());
    // And the only clear fields are the ones the hub routes on.
    let mut keys: Vec<&str> = back
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "body",
            "channel",
            "epoch",
            "recipient",
            "sender",
            "sent_ms",
            "seq",
            "sig",
            "stream_id",
            "v",
        ]
    );
    assert!(
        !delivered.data.contains(SECRET),
        "the request metadata must never be readable at the relay"
    );
    // Nothing of the stream reached the durable store, then or ever.
    drop(delivered);
    assert!(!rig.durable().contains(SECRET));
    assert_eq!(rig.frames.channels().unwrap().len(), 0);

    // The relay forgets the stream when either end says it is done, and tells
    // the other end so it stops waiting.
    let mut rx_a = rig.state.streams.connect(&rig.a.node_id());
    let (status, _) = rig
        .send(&rig.b, "DELETE", &format!("/v0/streams/{stream_id}"), "")
        .await;
    assert_eq!(status, StatusCode::OK);
    let note = rx_a.recv().await.unwrap();
    assert_eq!(note.event, "control");
    assert!(note.data.contains("peer_gone"), "{}", note.data);
    assert_eq!(rig.state.streams.open_streams(), 0);
}

/// The relay is a membership boundary, not only a pipe: the sender must hold
/// the channel and so must the recipient, checked against the hub's own
/// record rather than against anything in the frame.
#[tokio::test]
async fn both_ends_must_hold_the_channel() {
    let rig = build(StreamLimits::default(), &["@mesh"]);
    let _rx = rig.state.streams.connect(&rig.b.node_id());
    let env = rig.open_frame(&new_stream_id(), 0);
    let (status, body) = rig.post(&rig.a, &env).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // And a frame signed by someone other than the caller is refused before
    // anything is routed.
    let rig = build(StreamLimits::default(), &[CHANNEL]);
    let _rx = rig.state.streams.connect(&rig.b.node_id());
    let env = rig.open_frame(&new_stream_id(), 0);
    let (status, body) = rig.post(&rig.b, &env).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("not the authenticated key"));
}

/// A peer that is not connected is said so, rather than having frames held
/// for it: the relay is not a mailbox, and a stream is not durable.
#[tokio::test]
async fn an_unconnected_recipient_is_reported_not_buffered() {
    let rig = build(StreamLimits::default(), &[CHANNEL]);
    let env = rig.open_frame(&new_stream_id(), 0);
    let (status, body) = rig.post(&rig.a, &env).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["reason"], "not_connected");
    assert_eq!(rig.state.streams.open_streams(), 0);
}

/// A full per-stream buffer is backpressure: the sender is told to wait, the
/// stream stays open, and nothing is dropped. Reading one frame makes room
/// for exactly one more.
#[tokio::test]
async fn a_full_buffer_is_backpressure_and_reading_makes_room() {
    let rig = build(
        StreamLimits {
            per_stream_buffer: 2,
            ..Default::default()
        },
        &[CHANNEL],
    );
    let mut rx = rig.state.streams.connect(&rig.b.node_id());
    let stream_id = new_stream_id();
    for seq in 0..2 {
        let env = rig.frame(&stream_id, seq, &StreamFrame::chunk(b"x", false));
        assert_eq!(rig.post(&rig.a, &env).await.0, StatusCode::ACCEPTED);
    }
    let env = rig.frame(&stream_id, 2, &StreamFrame::chunk(b"x", false));
    let (status, body) = rig.post(&rig.a, &env).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["reason"], "backpressure");
    assert_eq!(
        rig.state.streams.open_streams(),
        1,
        "backpressure does not end the stream"
    );

    // Reading one frame releases exactly one slot. A fresh sequence number,
    // because the hub treats a repeated request signature as a replay.
    drop(rx.recv().await.unwrap());
    let env = rig.frame(&stream_id, 3, &StreamFrame::chunk(b"x", false));
    assert_eq!(rig.post(&rig.a, &env).await.0, StatusCode::ACCEPTED);
}

/// One member cannot open unbounded streams, and cannot post unbounded
/// frames. Both caps are per member, so one peer's traffic is not another's
/// problem.
#[tokio::test]
async fn concurrency_and_rate_are_bounded_per_member() {
    let rig = build(
        StreamLimits {
            max_open_per_member: 2,
            ..Default::default()
        },
        &[CHANNEL],
    );
    let _rx = rig.state.streams.connect(&rig.b.node_id());
    for _ in 0..2 {
        let env = rig.open_frame(&new_stream_id(), 0);
        assert_eq!(rig.post(&rig.a, &env).await.0, StatusCode::ACCEPTED);
    }
    let env = rig.open_frame(&new_stream_id(), 0);
    let (status, body) = rig.post(&rig.a, &env).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["reason"], "too_many_streams");

    let rig = build(
        StreamLimits {
            rate_per_min: 1,
            ..Default::default()
        },
        &[CHANNEL],
    );
    let _rx = rig.state.streams.connect(&rig.b.node_id());
    let stream_id = new_stream_id();
    assert_eq!(
        rig.post(&rig.a, &rig.open_frame(&stream_id, 0)).await.0,
        StatusCode::ACCEPTED
    );
    let (status, body) = rig
        .post(
            &rig.a,
            &rig.frame(&stream_id, 1, &StreamFrame::chunk(b"x", true)),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["reason"], "rate_limited");
}

/// A frame too large for the relay is refused by size before it is parsed,
/// so a sender cannot spend the hub's memory to find out it was too big.
#[tokio::test]
async fn an_oversized_frame_is_refused_by_size() {
    let rig = build(
        StreamLimits {
            max_frame_bytes: 256,
            ..Default::default()
        },
        &[CHANNEL],
    );
    let _rx = rig.state.streams.connect(&rig.b.node_id());
    let env = rig.frame(
        &new_stream_id(),
        0,
        &StreamFrame::chunk(&vec![b'x'; 4096], false),
    );
    let (status, _) = rig.post(&rig.a, &env).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

/// The far side opens the body with the key the hub never had, which is the
/// whole arrangement stated as one assertion.
#[tokio::test]
async fn the_recipient_opens_what_the_hub_could_not() {
    let rig = build(StreamLimits::default(), &[CHANNEL]);
    let mut rx = rig.state.streams.connect(&rig.b.node_id());
    let env = rig.open_frame(&new_stream_id(), 0);
    assert_eq!(rig.post(&rig.a, &env).await.0, StatusCode::ACCEPTED);
    let delivered = rx.recv().await.unwrap();
    let back: StreamEnvelope = serde_json::from_str(&delivered.data).unwrap();
    back.verify().unwrap();
    let frame = back.open(&rig.ring_b, &rig.b, Direction::Serving).unwrap();
    match frame {
        StreamFrame::Open(open) => assert_eq!(open.path, SECRET),
        other => panic!("expected an open, got {}", other.name()),
    }
}
