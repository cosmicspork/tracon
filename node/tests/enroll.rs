//! The enrollment flow end to end, in one process: a hub, an enrolled node
//! that invites, and a fresh node that accepts and ends up holding the keys.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::Arc;
use std::time::Duration;

use hub::store::{Member, MemberStore, MemoryMembers};
use proto::enroll::{sign_binding, EnrollRequest};
use proto::envelope::DataKey;
use proto::frame::MESH_CHANNEL;
use proto::keyring::Keyring;
use proto::keys::Identity;
use serde_json::json;
use tracon::mesh::enroll;
use tracon::store::Store;

struct Quiet;
impl enroll::Progress for Quiet {
    fn say(&self, _: &str) {}
}

/// A store holding genesis keyrings for `channels`, as an inviter does.
fn store_holding(id: &Identity, channels: &[&str]) -> Store {
    let store = Store::open_in_memory().unwrap();
    for c in channels {
        let ring = Keyring::genesis(&id.x25519_public(), &DataKey::generate());
        store.channel_put(c, &ring.to_bytes(), "{}").unwrap();
    }
    store
}

#[tokio::test]
async fn a_fresh_node_is_invited_admitted_and_handed_keys() {
    state::isolate();
    let a = Identity::from_seed(&[1u8; 32]);
    let b = Identity::from_seed(&[2u8; 32]);
    let hub = support::mesh::start_hub_personal(&[&a]).await;

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
    let req = tokio::time::timeout(support::mesh::WAIT, async {
        loop {
            if let Some(r) = enroll::poll_invite(&a, &hub, &inv.code).await.unwrap() {
                break r;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("B's enrollment request reached the hub");
    assert_eq!(req.node_id, b.node_id());
    assert_eq!(req.name, "laptop");
    // The fingerprint the operator compares covers the node id; the signature
    // in the slot is what ties the sealing key beside it to that same key.
    assert!(req.binding_ok());
    assert_eq!(req.binding_sig, sign_binding(&b));
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
        &req.binding_sig,
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
    state::isolate();
    let a = Identity::from_seed(&[1u8; 32]);
    let b = Identity::from_seed(&[2u8; 32]);
    let hub = support::mesh::start_hub_personal(&[&a]).await;
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
        &proto::enroll::sign_binding(&b),
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
    state::isolate();
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

/// A mesh that has been running for a while has a long `@mesh` history, and a
/// fresh node starts reading it at seq 0 while the handoff it is waiting for
/// sits at the head. Catching up must not cost a poll interval per page, or
/// the backlog alone spends the enrolment deadline.
#[tokio::test]
async fn a_long_mesh_backlog_does_not_delay_the_handoff() {
    state::isolate();
    let a = Identity::from_seed(&[1u8; 32]);
    let b = Identity::from_seed(&[2u8; 32]);
    // Four pages' worth at the reader's limit of 200.
    let hub = support::mesh::start_hub_with_backlog(
        &[(&a, &[MESH_CHANNEL, "personal"][..]), (&b, &[][..])],
        600,
    )
    .await;

    let a_store = Store::open_in_memory().unwrap();
    for c in [MESH_CHANNEL, "personal"] {
        let ring = Keyring::genesis(&a.x25519_public(), &DataKey::generate());
        a_store.channel_put(c, &ring.to_bytes(), "{}").unwrap();
    }

    let inv = enroll::open_invite(&a, &hub, &["personal".into()], Some(60))
        .await
        .unwrap();

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

    let req = tokio::time::timeout(support::mesh::WAIT, async {
        loop {
            if let Some(r) = enroll::poll_invite(&a, &hub, &inv.code).await.unwrap() {
                break r;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("B's enrollment request reached the hub");

    let admitted_at = tokio::time::Instant::now();
    enroll::admit(
        &a_store,
        &a,
        &hub,
        &req.node_id,
        &req.x25519_pub,
        &req.binding_sig,
        &req.name,
        &inv.channels,
        &[],
    )
    .await
    .unwrap();

    let got = accept.await.unwrap().unwrap();
    assert!(got.contains(&MESH_CHANNEL.to_string()));
    // One poll interval covers the wait for admission; the three further
    // pages of backlog must not each cost another.
    let took = admitted_at.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "the handoff took {took:?}; the backlog was walked a poll interval at a time"
    );
}

// --------------------------------------------------------------- key binding

/// A hub serving one fixed enrollment slot, however that slot was written.
async fn stub_slot(body: serde_json::Value) -> String {
    let app = axum::Router::new().route(
        "/v0/enroll/{code}",
        axum::routing::get(move || {
            let body = body.clone();
            async move { axum::Json(body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// The operator compares fingerprints of node ids, and nothing else. So a hub
/// that keeps the id it shows and substitutes the sealing key beside it would
/// have every channel keyring wrapped to a key it holds, with both fingerprints
/// still matching. The filler's signature over its own pair is what closes
/// that, and the inviter checks it before the request is even displayed.
#[tokio::test]
async fn a_slot_whose_sealing_key_its_node_id_never_signed_is_refused() {
    state::isolate();
    let a = Identity::from_seed(&[1u8; 32]);
    let b = Identity::from_seed(&[2u8; 32]);
    let c = Identity::from_seed(&[3u8; 32]);
    let honest = serde_json::to_value(EnrollRequest::signed(&b, "laptop", "x86_64")).unwrap();

    let hub = stub_slot(honest.clone()).await;
    let got = enroll::poll_invite(&a, &hub, "7KQ4M2XA")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.node_id, b.node_id());
    assert!(got.binding_ok());

    for forged in [
        // B's id, C's sealing key, B's own signature carried along.
        json!({"x25519_pub": c.x25519_hex()}),
        // A signature by the holder of the substituted key proves nothing
        // about the id the operator is comparing.
        json!({"x25519_pub": c.x25519_hex(), "binding_sig": sign_binding(&c)}),
        // A slot from before the proof existed.
        json!({"binding_sig": ""}),
        json!({"binding_sig": "00".repeat(64)}),
    ] {
        let mut slot = honest.clone();
        for (k, v) in forged.as_object().unwrap() {
            slot[k] = v.clone();
        }
        let hub = stub_slot(slot).await;
        let err = enroll::poll_invite(&a, &hub, "7KQ4M2XA").await.unwrap_err();
        assert!(
            matches!(&err, enroll::EnrollError::Local(m) if m.contains("not signed by its node id")),
            "{err}"
        );
    }
}

/// The same check again at the point it protects: nothing is wrapped, and no
/// member record is written, for a pair whose sealing key its node id never
/// claimed — whatever route the pair arrived by.
#[tokio::test]
async fn admit_refuses_a_sealing_key_its_node_id_never_signed() {
    state::isolate();
    let a = Identity::from_seed(&[1u8; 32]);
    let b = Identity::from_seed(&[2u8; 32]);
    let c = Identity::from_seed(&[3u8; 32]);
    let hub = support::mesh::start_hub_personal(&[&a]).await;
    let a_store = store_holding(&a, &[MESH_CHANNEL, "personal"]);

    for (x25519, sig) in [
        (c.x25519_hex(), sign_binding(&b)),
        (c.x25519_hex(), sign_binding(&c)),
        (b.x25519_hex(), sign_binding(&c)),
        (b.x25519_hex(), String::new()),
    ] {
        let err = enroll::admit(
            &a_store,
            &a,
            &hub,
            &b.node_id(),
            &x25519,
            &sig,
            "laptop",
            &["personal".into()],
            &[],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, enroll::EnrollError::Local(m) if m.contains("not signed by its node id")),
            "{err}"
        );
    }
    // Refused before the hub was touched at all: B is no member, so it cannot
    // even read the directory, and no handoff was posted for it.
    assert!(
        tracon::mesh::client::MeshClient::get_once(&b, &hub, "/v0/members")
            .await
            .is_err()
    );
    // B's own pair is admitted and handed the keys.
    enroll::admit(
        &a_store,
        &a,
        &hub,
        &b.node_id(),
        &b.x25519_hex(),
        &sign_binding(&b),
        "laptop",
        &["personal".into()],
        &[],
    )
    .await
    .unwrap();
    let members = tracon::mesh::client::MeshClient::get_once(&b, &hub, "/v0/members")
        .await
        .unwrap();
    assert_eq!(members.as_array().unwrap().len(), 2);
}

/// The hub's member directory is the hub's own answer, so a keyring is never
/// wrapped to a key in it that its owner has not signed for either — otherwise
/// a rewritten record collects the keys that enrollment refused to hand over.
#[tokio::test]
async fn rehanding_a_channel_skips_a_member_key_the_hub_cannot_prove() {
    state::isolate();
    let a = Identity::from_seed(&[1u8; 32]);
    let b = Identity::from_seed(&[2u8; 32]);
    let c = Identity::from_seed(&[3u8; 32]);
    let a_store = store_holding(&a, &[MESH_CHANNEL, "personal"]);
    let member = |id: &Identity, x25519: String, sig: String| Member {
        node_id: id.node_id(),
        x25519_pub: x25519,
        binding_sig: sig,
        name: "n".into(),
        channels: vec![MESH_CHANNEL.into(), "personal".into()],
        admitted_ms: 0,
        admitted_by: "test".into(),
        role: Default::default(),
    };

    // B's record carries C's sealing key under B's own signature.
    let members = Arc::new(MemoryMembers::new());
    members
        .put(&member(&a, a.x25519_hex(), sign_binding(&a)))
        .unwrap();
    members
        .put(&member(&b, c.x25519_hex(), sign_binding(&b)))
        .unwrap();
    let hub = support::mesh::start_hub_with_members(members).await;
    assert_eq!(
        enroll::rehand_channel(&a_store, &a, &hub, "personal")
            .await
            .unwrap(),
        0
    );

    // The record B signed for is handed the channel as usual.
    let members = Arc::new(MemoryMembers::new());
    members
        .put(&member(&a, a.x25519_hex(), sign_binding(&a)))
        .unwrap();
    members
        .put(&member(&b, b.x25519_hex(), sign_binding(&b)))
        .unwrap();
    let hub = support::mesh::start_hub_with_members(members).await;
    assert_eq!(
        enroll::rehand_channel(&a_store, &a, &hub, "personal")
            .await
            .unwrap(),
        1
    );
}
