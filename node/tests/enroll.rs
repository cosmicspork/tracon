//! The enrollment flow end to end, in one process: a hub, an enrolled node
//! that invites, and a fresh node that accepts and ends up holding the keys.

use std::sync::Arc;
use std::time::Duration;

use hub::store::{Member, MemberStore, MemoryFrames, MemoryMembers};
use hub::HubConfig;
use proto::envelope::DataKey;
use proto::frame::MESH_CHANNEL;
use proto::keyring::Keyring;
use proto::keys::Identity;
use tracon::mesh::enroll;
use tracon::store::Store;

struct Quiet;
impl enroll::Progress for Quiet {
    fn say(&self, _: &str) {}
}

async fn start_hub(admitted: &[&Identity]) -> String {
    let members = Arc::new(MemoryMembers::new());
    for id in admitted {
        members
            .put(&Member {
                node_id: id.node_id(),
                x25519_pub: id.x25519_hex(),
                name: "first".into(),
                channels: vec![MESH_CHANNEL.into(), "personal".into()],
                admitted_ms: 0,
                admitted_by: "env".into(),
            })
            .unwrap();
    }
    let app = hub::app(Arc::new(MemoryFrames::new()), members, HubConfig::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn a_fresh_node_is_invited_admitted_and_handed_keys() {
    let a = Identity::from_seed(&[1u8; 32]);
    let b = Identity::from_seed(&[2u8; 32]);
    let hub = start_hub(&[&a]).await;

    // A holds keys for @mesh and personal.
    let a_store = Store::open_in_memory().unwrap();
    for c in [MESH_CHANNEL, "personal"] {
        let ring = Keyring::genesis(&a.x25519_public(), &DataKey::generate());
        a_store.channel_put(c, &ring.to_bytes(), "{}").unwrap();
    }

    let inv = enroll::open_invite(&a, &hub, &["personal".into()], Some(60))
        .await
        .unwrap();
    assert_eq!(inv.code.len(), 8);
    assert_eq!(inv.display_code().len(), 9);
    assert!(inv.url.ends_with(&format!("/#enroll={}", inv.code)));
    assert!(enroll::poll_invite(&a, &hub, &inv.code)
        .await
        .unwrap()
        .is_none());

    // B accepts in the background: it posts its keys and waits for keys.
    let b_store = Arc::new(Store::open_in_memory().unwrap());
    let (hub2, code2, store2) = (hub.clone(), inv.code.clone(), b_store.clone());
    let accept = tokio::spawn(async move {
        let b = Identity::from_seed(&[2u8; 32]);
        enroll::accept(
            store2,
            &b,
            &hub2,
            &code2,
            "laptop",
            "x86_64",
            Duration::from_secs(30),
            &Quiet,
        )
        .await
    });

    // A sees the request, checks the fingerprint, admits.
    let req = loop {
        if let Some(r) = enroll::poll_invite(&a, &hub, &inv.code).await.unwrap() {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(req.node_id, b.node_id());
    assert_eq!(req.name, "laptop");
    assert_eq!(
        proto::enroll::fingerprint_hex(&req.node_id).unwrap(),
        proto::enroll::fingerprint(&b.verifying_key().to_bytes())
    );
    // Consumed.
    assert!(matches!(
        enroll::poll_invite(&a, &hub, &inv.code).await,
        Err(enroll::EnrollError::Refused { status: 404, .. })
    ));
    enroll::admit(
        &a_store,
        &a,
        &hub,
        &req.node_id,
        &req.x25519_pub,
        &req.name,
        &inv.channels,
        &[],
    )
    .await
    .unwrap();

    let got = accept.await.unwrap().unwrap();
    assert!(got.contains(&MESH_CHANNEL.to_string()) && got.contains(&"personal".to_string()));
    // B can now open the newest epoch of both channels.
    for c in [MESH_CHANNEL, "personal"] {
        let row = b_store.channel_get(c).unwrap().unwrap();
        let ring = Keyring::from_bytes(&row.keyring).unwrap();
        assert!(ring.key_for(ring.newest(), &b).is_ok());
        assert!(ring.key_for(ring.newest(), &a).is_err());
    }
    assert_eq!(
        b_store.node_channels(&b.node_id()).unwrap(),
        vec!["@mesh", "personal"]
    );
    // A recorded B locally.
    assert_eq!(
        a_store.node_channels(&b.node_id()).unwrap(),
        vec!["@mesh", "personal"]
    );
    assert!(a_store.get_node(&b.node_id()).unwrap().is_some());

    // B is a member: it can list members and sees itself and A.
    let members = tracon::mesh::client::MeshClient::get_once(&b, &hub, "/v0/members")
        .await
        .unwrap();
    assert_eq!(members.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn admit_refuses_channels_this_node_cannot_hand_off() {
    let a = Identity::from_seed(&[1u8; 32]);
    let b = Identity::from_seed(&[2u8; 32]);
    let hub = start_hub(&[&a]).await;
    let a_store = Store::open_in_memory().unwrap();
    let ring = Keyring::genesis(&a.x25519_public(), &DataKey::generate());
    a_store
        .channel_put(MESH_CHANNEL, &ring.to_bytes(), "{}")
        .unwrap();
    let err = enroll::admit(
        &a_store,
        &a,
        &hub,
        &b.node_id(),
        &b.x25519_hex(),
        "b",
        &["work".into()],
        &[],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, enroll::EnrollError::Local(m) if m.contains("work")));
    // Nothing was admitted on the hub.
    assert!(
        tracon::mesh::client::MeshClient::get_once(&b, &hub, "/v0/members")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn key_handoff_merges_epochs() {
    let a = Identity::from_seed(&[1u8; 32]);
    let b = Identity::from_seed(&[2u8; 32]);
    let store = Store::open_in_memory().unwrap();
    let ring = Keyring::genesis(&a.x25519_public(), &DataKey::generate());
    let (rotated, _) = ring.rotate(&a.x25519_public(), 1000);
    let first = ring.wrap_for(&a, &b.x25519_public()).unwrap();
    let second = rotated.wrap_for(&a, &b.x25519_public()).unwrap();
    let n = enroll::apply_key_handoff(
        &store,
        &b.node_id(),
        &[proto::frame::ChannelHandoff {
            name: "personal".into(),
            keyring: first,
            bindings_json: "{}".into(),
        }],
    );
    assert_eq!(n, 1);
    enroll::apply_key_handoff(
        &store,
        &b.node_id(),
        &[proto::frame::ChannelHandoff {
            name: "personal".into(),
            keyring: second,
            bindings_json: "{}".into(),
        }],
    );
    let row = store.channel_get("personal").unwrap().unwrap();
    let merged = Keyring::from_bytes(&row.keyring).unwrap();
    assert_eq!(merged.entries().len(), 2);
    assert!(!merged.newest().is_genesis());
}
