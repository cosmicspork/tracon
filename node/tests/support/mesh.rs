//! Two-node mesh scaffolding shared by the hub-backed tests: identities from
//! seeds, an in-process hub router, the keyrings a pair of nodes holds, and a
//! condition wait with one budget for every test.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use hub::store::{Member, MemberStore, MemoryFrames, MemoryMembers};
use hub::HubConfig;
use proto::envelope::DataKey;
use proto::frame::MESH_CHANNEL;
use proto::keyring::Keyring;
use proto::keys::Identity;

/// Long enough for a slow CI runner to relay through the hub several times;
/// short enough that a genuine hang fails the test rather than the job.
pub const WAIT: Duration = Duration::from_secs(20);

pub fn identity(seed: u8) -> Identity {
    Identity::from_seed(&[seed; 32])
}

/// A hub with these members already admitted, on an ephemeral port.
pub async fn start_hub(admitted: &[(&Identity, &[&str])]) -> String {
    let members = Arc::new(MemoryMembers::new());
    for (id, channels) in admitted {
        members
            .put(&Member {
                node_id: id.node_id(),
                x25519_pub: id.x25519_hex(),
                name: "n".into(),
                channels: channels.iter().map(|s| s.to_string()).collect(),
                admitted_ms: 0,
                admitted_by: "test".into(),
                role: Default::default(),
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

/// A hub where every member is in `@mesh` and `personal`.
pub async fn start_hub_personal(admitted: &[&Identity]) -> String {
    let channels: &[&str] = &[MESH_CHANNEL, "personal"];
    let admitted: Vec<(&Identity, &[&str])> = admitted.iter().map(|id| (*id, channels)).collect();
    start_hub(&admitted).await
}

/// Keyrings for a pair: A holds genesis rings, B holds A's handoffs of them.
pub struct Rings {
    pub a: Vec<(String, Keyring)>,
    pub b: Vec<(String, Keyring)>,
}

pub fn rings_for(a: &Identity, b: &Identity, channels: &[&str]) -> Rings {
    let mut out = Rings {
        a: Vec::new(),
        b: Vec::new(),
    };
    for c in std::iter::once(&MESH_CHANNEL).chain(channels) {
        let ring = Keyring::genesis(&a.x25519_public(), &DataKey::generate());
        out.b
            .push((c.to_string(), ring.wrap_for(a, &b.x25519_public()).unwrap()));
        out.a.push((c.to_string(), ring));
    }
    out
}

/// Poll `f` until it holds or `WAIT` elapses.
pub async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if f() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
