//! Regenerates `spec/vectors/*.json`. Run from the workspace root:
//!
//! ```sh
//! cargo run -p tracon-proto --example vectors
//! ```
//!
//! The unit tests assert the committed files match, so a derivation or label
//! change must be accompanied by regenerated vectors — deliberately.

use std::fs;
use std::path::Path;

use proto::auth::{sign_request, AuthRequest};
use proto::envelope::{seal_to_with_ephemeral, wrap_key_with_ephemeral, DataKey};
use proto::frame::Payload;
use proto::keyring::Keyring;
use proto::keys::Identity;
use proto::CONTRACT_VERSION;
use serde_json::{json, Value};

fn seed(n: u8) -> [u8; 32] {
    [n; 32]
}
fn nonce(n: u8) -> [u8; 24] {
    [n; 24]
}
fn hx(b: &[u8]) -> String {
    hex::encode(b)
}

fn main() {
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../spec/vectors");
    fs::create_dir_all(&out).unwrap();

    // key-derivation
    let mut vectors = Vec::new();
    for n in [0u8, 1, 0x42, 0xff] {
        let id = Identity::from_seed(&seed(n));
        vectors.push(json!({
            "seed_hex": hx(&seed(n)),
            "x25519_public_hex": id.x25519_hex(),
            "ed25519_public_hex": hx(&id.verifying_key().to_bytes()),
            "node_id": id.node_id(),
            "credential_store_key_hex": hx(&id.credential_store_key().to_bytes()),
        }));
    }
    write(
        &out,
        "key-derivation.json",
        json!({"contract_version": CONTRACT_VERSION, "vectors": vectors}),
    );

    // envelope
    let key = DataKey::from_bytes(seed(0x11));
    let sealing: Vec<Value> = [(b"" as &[u8], b"" as &[u8]), (b"frame body", b"personal\x1f0000000000000000")]
        .iter()
        .enumerate()
        .map(|(i, (pt, aad))| {
            let s = key.seal_with_nonce(nonce(0x20 + i as u8), pt, aad);
            json!({"key_hex": hx(&key.to_bytes()), "nonce_hex": hx(&nonce(0x20 + i as u8)), "aad_hex": hx(aad), "plaintext_hex": hx(pt), "sealed_hex": hx(&s.to_bytes())})
        })
        .collect();
    let r = Identity::from_seed(&seed(0x33));
    let dk = DataKey::from_bytes(seed(0x44));
    let w = wrap_key_with_ephemeral(&r.x25519_public(), &dk, seed(0x55), nonce(0x66));
    let wrapping = vec![
        json!({"recipient_seed_hex": hx(&seed(0x33)), "ephemeral_secret_hex": hx(&seed(0x55)), "nonce_hex": hx(&nonce(0x66)), "data_key_hex": hx(&dk.to_bytes()), "wrapped_hex": hx(&w.to_bytes())}),
    ];
    let b = seal_to_with_ephemeral(
        &r.x25519_public(),
        b"{\"kind\":\"hello\"}",
        b"@mesh\x1fsr",
        seed(0x77),
        nonce(0x88),
    );
    let boxing = vec![
        json!({"recipient_seed_hex": hx(&seed(0x33)), "ephemeral_secret_hex": hx(&seed(0x77)), "nonce_hex": hx(&nonce(0x88)), "aad_hex": hx(b"@mesh\x1fsr"), "plaintext_hex": hx(b"{\"kind\":\"hello\"}"), "boxed_hex": hx(&b.to_bytes())}),
    ];
    write(
        &out,
        "envelope.json",
        json!({"contract_version": CONTRACT_VERSION, "sealing": sealing, "wrapping": wrapping, "boxing": boxing}),
    );

    // auth
    let signer = Identity::from_seed(&seed(0x99));
    let requests: Vec<Value> = [
        ("GET", "/v0/frames?channel=personal&after=41", b"" as &[u8], 1_787_000_000u64),
        ("POST", "/v0/frames", b"{\"v\":1}", 1_787_000_001),
    ]
    .iter()
    .map(|(m, p, body, ts)| {
        let req = AuthRequest::new(m, p, body, *ts);
        json!({"method": m, "path": p, "body_hex": hx(body), "timestamp": ts, "signer_seed_hex": hx(&seed(0x99)), "canon_hex": hx(&req.signing_bytes()), "public_key_hex": signer.node_id(), "signature_hex": hx(&sign_request(&signer, &req))})
    })
    .collect();
    write(
        &out,
        "auth.json",
        json!({"contract_version": CONTRACT_VERSION, "requests": requests}),
    );

    // keyring
    let owner = Identity::from_seed(&seed(0xa1));
    let grantee = Identity::from_seed(&seed(0xa2));
    let k0 = DataKey::from_bytes(seed(0xb0));
    let k1 = DataKey::from_bytes(seed(0xb1));
    let rid = [0xc1u8; 16];
    let ring = Keyring::genesis_with(&owner.x25519_public(), &k0, seed(0xd0), nonce(0xe0))
        .rotate_with(
            &owner.x25519_public(),
            &k1,
            rid,
            1_700_000_000_000,
            seed(0xd1),
            nonce(0xe1),
        );
    let eph = [(seed(0xd2), nonce(0xe2)), (seed(0xd3), nonce(0xe3))];
    let handoff = ring
        .wrap_for_with(&owner, &grantee.x25519_public(), &eph)
        .unwrap();
    write(
        &out,
        "keyring.json",
        json!({
            "contract_version": CONTRACT_VERSION,
            "owner_seed_hex": hx(&seed(0xa1)), "grantee_seed_hex": hx(&seed(0xa2)),
            "genesis_key_hex": hx(&k0.to_bytes()), "genesis_ephemeral_hex": hx(&seed(0xd0)), "genesis_nonce_hex": hx(&nonce(0xe0)),
            "rotated_key_hex": hx(&k1.to_bytes()), "rotated_id_hex": hx(&rid), "rotated_created_at": 1_700_000_000_000i64,
            "rotated_ephemeral_hex": hx(&seed(0xd1)), "rotated_nonce_hex": hx(&nonce(0xe1)),
            "ring_hex": hx(&ring.to_bytes()),
            "handoff_ephemerals": eph.iter().map(|(e, n)| (hx(e), hx(n))).collect::<Vec<_>>(),
            "handoff_hex": hx(&handoff.to_bytes()),
        }),
    );

    // frame: canonical bytes and ids are pinned via a test-only path; the
    // example records the inputs and the expected outputs computed here by the
    // same code (the test recomputes them independently from the inputs).
    let s = Identity::from_seed(&seed(0xf1));
    let r = Identity::from_seed(&seed(0xf2));
    let payload = Payload::Hello {
        node: json!({"name": "bazzite"}),
        contract: CONTRACT_VERSION,
    };
    let (channel_canon, channel_id, direct_canon, direct_id, prefix) =
        proto::frame::vector_support(&s, &r, "personal", 1_787_000_000_000, &payload);
    write(
        &out,
        "frame.json",
        json!({
            "contract_version": CONTRACT_VERSION,
            "sender_seed_hex": hx(&seed(0xf1)), "recipient_seed_hex": hx(&seed(0xf2)),
            "channel": "personal", "sent_ms": 1_787_000_000_000i64,
            "payload": serde_json::to_value(&payload).unwrap(),
            "channel_key_hex": hx(&seed(0xf3)),
            "channel_epoch_hex": hx(&[0u8; 16]),
            "channel_canon_hex": hx(&channel_canon), "channel_id_hex": hx(&channel_id),
            "direct_canon_hex": hx(&direct_canon), "direct_id_hex": hx(&direct_id),
            "signing_prefix_hex": hx(&prefix),
        }),
    );
}

fn write(dir: &Path, name: &str, v: Value) {
    let path = dir.join(name);
    fs::write(&path, serde_json::to_string_pretty(&v).unwrap() + "\n").unwrap();
    println!("wrote {}", path.display());
}
