//! Two-node mesh scaffolding shared by the hub-backed tests: identities from
//! seeds, an in-process hub router, the keyrings a pair of nodes holds, and a
//! condition wait with one budget for every test.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use hub::store::{FrameStore, Member, MemberStore, MemoryFrames, MemoryMembers};
use hub::HubConfig;
use proto::frame::MESH_CHANNEL;
use proto::keys::Identity;

/// Long enough for a slow CI runner to relay through the hub several times;
/// short enough that a genuine hang fails the test rather than the job.
pub const WAIT: Duration = Duration::from_secs(20);

pub fn identity(seed: u8) -> Identity {
    Identity::from_seed(&[seed; 32])
}

/// A hub whose stores the test keeps in hand, so a run can revoke a member or
/// plant a frame no honest node would post.
pub struct TestHub {
    pub url: String,
    pub members: Arc<MemoryMembers>,
    pub frames: Arc<MemoryFrames>,
    /// The router's own state.
    pub state: hub::AppState,
}

/// A hub with these members already admitted, on an ephemeral port.
pub async fn start_hub(admitted: &[(&Identity, &[&str])]) -> String {
    start_hub_with_backlog(admitted, 0).await
}

/// As `start_hub`, with `backlog` frames already on `@mesh`. They are not
/// valid envelopes — a reader skips them — but they occupy sequence numbers,
/// which is what a node catching up from seq 0 has to walk.
pub async fn start_hub_with_backlog(admitted: &[(&Identity, &[&str])], backlog: usize) -> String {
    start_hub_managed(admitted, backlog).await.url
}

/// As `start_hub_with_backlog`, handing back the stores as well.
pub async fn start_hub_managed(admitted: &[(&Identity, &[&str])], backlog: usize) -> TestHub {
    let members = Arc::new(MemoryMembers::new());
    for (id, channels) in admitted {
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
    for _ in 0..backlog {
        frames.append(MESH_CHANNEL, "{}", 0).unwrap();
    }
    serve(frames, members).await
}

/// A hub over a member table the test wrote itself, for records the admit
/// route would refuse to write — a sealing key its owner never signed for.
pub async fn start_hub_with_members(members: Arc<MemoryMembers>) -> String {
    serve(Arc::new(MemoryFrames::new()), members).await.url
}

async fn serve(frames: Arc<MemoryFrames>, members: Arc<MemoryMembers>) -> TestHub {
    let state = hub::state_for(
        frames.clone(),
        members.clone(),
        HubConfig::default(),
        Arc::new(hub::pokes::PokeHub::new()),
        None,
    );
    let app = hub::app_with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestHub {
        url: format!("http://{addr}"),
        members,
        frames,
        state,
    }
}

/// A hub where every member is in `@mesh` and `personal`.
pub async fn start_hub_personal(admitted: &[&Identity]) -> String {
    let channels: &[&str] = &[MESH_CHANNEL, "personal"];
    let admitted: Vec<(&Identity, &[&str])> = admitted.iter().map(|id| (*id, channels)).collect();
    start_hub(&admitted).await
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
