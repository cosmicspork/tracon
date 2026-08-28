//! A snapshot round trip: a hub with a replica and a shared channel is
//! sealed to a restore key, restored into a fresh directory, and reopens with
//! its identity, keyrings, rows, and frames intact — and the wrong key opens
//! nothing.

use std::sync::Arc;

use hub::pokes::PokeHub;
use hub::replica::Replica;
use hub::snapshot::{self, FsObjects, ObjectStore};
use hub::store::{FrameStore, FsFrames, FsMembers, Member, MemberRole, MemberStore};
use proto::envelope::DataKey;
use proto::frame::{ChangeOp, MESH_CHANNEL};
use proto::keyring::Keyring;
use proto::keys::Identity;
use serde_json::json;

#[test]
fn snapshot_then_restore_reopens_the_same_hub() {
    let base = std::env::temp_dir().join(format!("tracon-snap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let data = base.join("data");
    std::fs::create_dir_all(&data).unwrap();

    // A hub with an identity, a member, a shared channel, a row, and a frame.
    let (identity, fresh) = hub::identity::load_or_generate(&data).unwrap();
    assert!(fresh);
    let hub_id = identity.node_id();
    let frames: Arc<dyn FrameStore> = Arc::new(FsFrames::new(&data).unwrap());
    let members: Arc<dyn MemberStore> = Arc::new(FsMembers::new(&data).unwrap());
    let a = Identity::from_seed(&[1u8; 32]);
    members
        .put(&Member {
            node_id: a.node_id(),
            x25519_pub: a.x25519_hex(),
            name: "a".into(),
            channels: vec![MESH_CHANNEL.into(), "personal".into()],
            admitted_ms: 0,
            admitted_by: "t".into(),
            role: MemberRole::Node,
        })
        .unwrap();
    let replica = Replica::open(
        &data,
        identity,
        frames.clone(),
        members.clone(),
        Arc::new(PokeHub::new()),
    )
    .unwrap();
    hub::admit_self(members.as_ref(), &replica, 0).unwrap();
    // Hand the replica a keyring directly (as a handoff would) so it can author.
    let ring = Keyring::genesis(&replica_pk(&replica), &DataKey::generate());
    replica.with_db(|c| {
        c.execute(
            "INSERT INTO replica_channel (name, keyring, bindings_json) VALUES ('personal', ?1, '{\"processing\":\"hub\"}')",
            [ring.to_bytes()],
        )
        .unwrap();
    });
    replica
        .write_change("personal", "memory", ChangeOp::Upsert, "m1", json!({"channel": "personal", "scope": "global", "kind": "fact", "body": "survives a restore", "confidence": 1.0, "state": "active", "created_ms": 1, "updated_ms": 1}))
        .unwrap();
    assert_eq!(frames.read("personal", 0, 10).unwrap().frames.len(), 1);

    // The restore key lives with the operator; the hub keeps the public half.
    let (seed_hex, pub_path) = snapshot::create_restore_key(&data).unwrap();
    assert!(pub_path.exists());
    let recipient = snapshot::recipient(&data).unwrap();
    let bucket = FsObjects::new(&base.join("bucket")).unwrap();
    let key = snapshot::take(&data, &recipient, &bucket, "tracon-hub", 1_000).unwrap();
    assert!(key.starts_with("tracon-hub/snapshot-1000"));
    // Ciphertext: nothing legible.
    let blob = bucket.get(&key).unwrap();
    assert!(!String::from_utf8_lossy(&blob).contains("survives a restore"));
    // Two more and a prune keep the newest two.
    snapshot::take(&data, &recipient, &bucket, "tracon-hub", 2_000).unwrap();
    snapshot::take(&data, &recipient, &bucket, "tracon-hub", 3_000).unwrap();
    let removed = snapshot::prune(&bucket, "tracon-hub", 2).unwrap();
    assert_eq!(removed, vec![key]);
    let latest = snapshot::latest(&bucket, "tracon-hub").unwrap().unwrap();
    assert!(latest.contains("3000"));

    // The wrong seed opens nothing; the right one restores everything.
    let into = base.join("restored");
    let wrong = hex::encode([7u8; 32]);
    assert!(snapshot::restore(&bucket, &latest, &wrong, &into).is_err());
    let written = snapshot::restore(&bucket, &latest, &seed_hex, &into).unwrap();
    assert!(written.iter().any(|p| p == "hub.db"));
    assert!(written.iter().any(|p| p == "hub-identity.seed"));
    assert!(written.iter().any(|p| p.starts_with("members/")));
    assert!(written.iter().any(|p| p.starts_with("frames/personal/")));

    let (identity2, fresh2) = hub::identity::load_or_generate(&into).unwrap();
    assert!(!fresh2);
    assert_eq!(identity2.node_id(), hub_id);
    let frames2: Arc<dyn FrameStore> = Arc::new(FsFrames::new(&into).unwrap());
    let members2: Arc<dyn MemberStore> = Arc::new(FsMembers::new(&into).unwrap());
    assert_eq!(frames2.read("personal", 0, 10).unwrap().frames.len(), 1);
    assert_eq!(members2.list().unwrap().len(), 2);
    let replica2 = Replica::open(
        &into,
        identity2,
        frames2,
        members2,
        Arc::new(PokeHub::new()),
    )
    .unwrap();
    assert_eq!(replica2.readable_channels(), vec!["personal".to_string()]);
    let body: String = replica2.with_db(|c| {
        c.query_row("SELECT body FROM memory WHERE id = 'm1'", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(body, "survives a restore");
    // And it can still author under the restored keyring.
    replica2
        .write_change("personal", "memory", ChangeOp::Upsert, "m2", json!({"channel": "personal", "scope": "global", "kind": "fact", "body": "after", "confidence": 1.0, "state": "active", "created_ms": 2, "updated_ms": 2}))
        .unwrap();
    let _ = std::fs::remove_dir_all(&base);
}

fn replica_pk(r: &Replica) -> x25519_dalek::PublicKey {
    proto::keys::key32(&r.x25519_hex())
        .map(x25519_dalek::PublicKey::from)
        .unwrap()
}
