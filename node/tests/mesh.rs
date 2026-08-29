//! Two nodes and a real hub router, all in one process. The clients' loops are
//! not spawned; the test drives `drain_once` / `pull_once` so every step is
//! deterministic.

use std::sync::Arc;

use hub::store::{Member, MemberStore, MemoryFrames, MemoryMembers};
use hub::HubConfig;
use proto::envelope::DataKey;
use proto::frame::{Payload, MESH_CHANNEL};
use proto::keyring::Keyring;
use proto::keys::Identity;
use serde_json::json;
use tracon::config::Config;
use tracon::mesh::client::MeshClient;
use tracon::mesh::HubState;
use tracon::store::{now_ms, NewEvent, NodeRow, SessionRow, Store};
use tracon::stream::{Bus, Frame};

#[path = "support/state.rs"]
mod state;

struct Node {
    id: Identity,
    store: Arc<Store>,
    bus: Bus,
    client: Arc<MeshClient>,
}

/// The next frame that is not the hub-state banner update.
fn next_frame(rx: &mut tokio::sync::broadcast::Receiver<Frame>) -> Frame {
    loop {
        let f = rx.try_recv().expect("a frame");
        if !matches!(f, Frame::Mesh(_)) {
            return f;
        }
    }
}

fn node_row(id: &Identity, name: &str) -> NodeRow {
    NodeRow {
        id: id.node_id(),
        name: name.into(),
        state: "ready".into(),
        failed_check: None,
        failed_detail: None,
        harness_id: "fake".into(),
        harness_pinned: "1".into(),
        harness_found: Some("1".into()),
        models_json: Some("[]".into()),
        checked_at_ms: Some(now_ms()),
        is_self: 1,
        x25519_pub: Some(id.x25519_hex()),
        last_seen_ms: None,
        reachable: 1,
    }
}

fn session(node: &str, id: &str, channel: &str) -> SessionRow {
    SessionRow {
        id: id.into(),
        node_id: node.into(),
        channel: channel.into(),
        work_item_id: None,
        repo_path: "/r".into(),
        worktree_path: None,
        branch: "b".into(),
        harness_id: "fake".into(),
        harness_version: "1".into(),
        harness_session_id: None,
        container_name: None,
        model: "m".into(),
        project_id: None,
        phase: "execute".into(),
        policy_version: None,
        review_id: None,
        budget_tokens: 10,
        tokens_used: 0,
        cost_usd: None,
        context_used: None,
        context_size: None,
        state: "running".into(),
        end_reason: None,
        last_error: None,
        turn_active: 0,
        draft: None,
        draft_updated_ms: None,
        created_ms: 1,
        started_mono_ms: None,
        ended_mono_ms: None,
        updated_ms: 1,
    }
}

async fn start_hub(admitted: &[(&Identity, &[&str])]) -> String {
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

fn node(seed: u8, name: &str, hub: &str, rings: &[(&str, Keyring)]) -> Node {
    let id = Identity::from_seed(&[seed; 32]);
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.put_node(&node_row(&id, name)).unwrap();
    for (c, ring) in rings {
        store.channel_put(c, &ring.to_bytes(), "{}").unwrap();
        store.node_channel_add(&id.node_id(), c).unwrap();
    }
    let bus = Bus::new();
    let mut cfg = Config::default();
    cfg.mesh.hub_url = Some(hub.to_string());
    let client = MeshClient::new(
        Identity::from_seed(&[seed; 32]),
        hub,
        store.clone(),
        bus.clone(),
        Arc::new(cfg),
        Default::default(),
    );
    Node {
        id,
        store,
        bus,
        client,
    }
}

/// Two nodes sharing `@mesh` and `personal`; B's rings are handoffs from A.
async fn pair() -> (Node, Node) {
    let a_id = Identity::from_seed(&[1u8; 32]);
    let b_id = Identity::from_seed(&[2u8; 32]);
    let hub = start_hub(&[
        (&a_id, &[MESH_CHANNEL, "personal", "secret"]),
        (&b_id, &[MESH_CHANNEL, "personal", "secret"]),
    ])
    .await;
    let mesh_ring = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let personal_ring = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let secret_ring = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let b_mesh = mesh_ring.wrap_for(&a_id, &b_id.x25519_public()).unwrap();
    let b_personal = personal_ring
        .wrap_for(&a_id, &b_id.x25519_public())
        .unwrap();
    // B is a hub member of `secret` but was never handed its key.
    let b_secret = Keyring::genesis(&b_id.x25519_public(), &DataKey::generate());
    let a = node(
        1,
        "alpha",
        &hub,
        &[
            (MESH_CHANNEL, mesh_ring),
            ("personal", personal_ring),
            ("secret", secret_ring),
        ],
    );
    let b = node(
        2,
        "beta",
        &hub,
        &[
            (MESH_CHANNEL, b_mesh),
            ("personal", b_personal),
            ("secret", b_secret),
        ],
    );
    (a, b)
}

#[tokio::test]
async fn hello_makes_a_peer_visible_and_presence_ages_it_out() {
    state::isolate();
    let (a, b) = pair().await;
    let mut b_sub = b.bus.subscribe();
    a.client.hello().await.unwrap();
    assert_eq!(b.client.pull_once().await.unwrap(), 1);
    let nodes = b.store.list_nodes().unwrap();
    let peer = nodes.iter().find(|n| n.id == a.id.node_id()).unwrap();
    assert_eq!(peer.name, "alpha");
    assert_eq!(peer.is_self, 0);
    assert_eq!(peer.reachable, 1);
    assert!(peer.last_seen_ms.is_some());
    let f = next_frame(&mut b_sub);
    assert!(matches!(f, Frame::Node(ref v) if v["id"] == a.id.node_id() && v["is_self"] == false));
    assert!(matches!(b.client.snapshot().hub, HubState::Connected));

    // Long after the last hello, the peer dims; the change is published.
    b.client.presence_tick(now_ms() + 3 * 60_000 + 1);
    let peer = b.store.get_node(&a.id.node_id()).unwrap().unwrap();
    assert_eq!(peer.reachable, 0);
    let f = next_frame(&mut b_sub);
    assert!(matches!(f, Frame::Node(ref v) if v["reachable"] == false));
    // A's own hello echoed back changes nothing on A.
    assert_eq!(a.client.pull_once().await.unwrap(), 0);
}

#[tokio::test]
async fn sessions_and_events_mirror_once_and_reach_the_bus() {
    state::isolate();
    let (a, b) = pair().await;
    let mut b_sub = b.bus.subscribe();
    let row = session(&a.id.node_id(), "s1", "personal");
    a.store.insert_session(&row).unwrap();
    a.client.on_frame(&Frame::Session(Box::new(row.clone())));
    let ev = Frame::Event {
        seq: 7,
        node_id: a.id.node_id(),
        session_id: "s1".into(),
        kind: "message".into(),
        ref_id: None,
        payload: json!({"text": "hi"}),
        at_ms: 5,
    };
    a.client.on_frame(&ev);
    a.client.on_frame(&ev); // the same persisted event, published twice
    assert_eq!(a.client.snapshot().queued, 3);
    assert_eq!(a.client.drain_once().await.unwrap(), 3);
    assert_eq!(a.client.snapshot().queued, 0);

    let applied = b.client.pull_once().await.unwrap();
    eprintln!("B snapshot after pull: {:?}", b.client.snapshot());
    eprintln!(
        "B cursors: {:?} {:?}",
        b.store.cursor_get("personal"),
        b.store.cursor_get("@mesh")
    );
    assert_eq!(applied, 2);
    let mirrored = b.store.get_session("s1").unwrap().unwrap();
    assert_eq!(mirrored.node_id, a.id.node_id());
    assert_eq!(mirrored.channel, "personal");
    let events = b.store.events_after("s1", -1, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].node_id, a.id.node_id());
    assert!(matches!(next_frame(&mut b_sub), Frame::Session(_)));
    assert!(
        matches!(b_sub.try_recv().unwrap(), Frame::Event { node_id, seq, .. } if node_id == a.id.node_id() && seq == 1)
    );
    assert!(b_sub.try_recv().is_err());
    // Nothing mirrored went back out.
    assert_eq!(b.client.snapshot().queued, 0);

    // A pull with nothing new is a no-op, and a re-pull after a cursor reset
    // is deduplicated by frame id.
    assert_eq!(b.client.pull_once().await.unwrap(), 0);
    b.store.cursor_set("personal", 0).unwrap();
    assert_eq!(b.client.pull_once().await.unwrap(), 0);
    assert_eq!(b.store.events_after("s1", -1, 10).unwrap().len(), 1);
}

#[tokio::test]
async fn a_peer_cannot_speak_for_another_node_and_unknown_keys_are_counted() {
    state::isolate();
    let (a, b) = pair().await;
    // A claims a session belongs to B.
    let row = session(&b.id.node_id(), "forged", "personal");
    a.client
        .enqueue("personal", None, &Payload::Session(json!(row)))
        .unwrap();
    // A also sends on a channel B has the wrong key for.
    a.client
        .enqueue(
            "secret",
            None,
            &Payload::Node(json!({"id": a.id.node_id()})),
        )
        .unwrap();
    a.client.drain_once().await.unwrap();
    assert_eq!(b.client.pull_once().await.unwrap(), 0);
    assert!(b.store.get_session("forged").unwrap().is_none());
    let s = b.client.snapshot();
    assert_eq!(s.undecryptable, 1);
    assert!(s
        .last_refusal
        .as_deref()
        .unwrap()
        .contains("spoke for another node"));
}

#[tokio::test]
async fn queue_frames_expire_answered_requests_and_snapshots_close_lost_sessions() {
    state::isolate();
    let (a, b) = pair().await;
    let row = session(&a.id.node_id(), "s1", "personal");
    a.store.insert_session(&row).unwrap();
    a.client.on_frame(&Frame::Session(Box::new(row.clone())));
    let perm = tracon::store::PermissionRow {
        id: "p1".into(),
        session_id: "s1".into(),
        node_id: a.id.node_id(),
        rpc_id: 1,
        tool_call_id: None,
        title: "Run tests".into(),
        kind: Some("shell".into()),
        raw_input: None,
        options: "[]".into(),
        state: "new".into(),
        answer_option_id: None,
        created_ms: 1,
        created_mono_ms: 0,
        resolved_mono_ms: None,
        expires_ms: now_ms() + 60_000,
    };
    a.store.insert_permission(&perm).unwrap();
    a.client.on_frame(&Frame::Queue {
        waiting: vec![perm.clone()],
    });
    a.client.drain_once().await.unwrap();
    b.client.pull_once().await.unwrap();
    assert_eq!(b.store.open_permissions().unwrap().len(), 1);

    // Answered on A: the next queue frame no longer lists it.
    a.store
        .resolve_permission("p1", "answered", Some("allow"), 1)
        .unwrap();
    a.client.on_frame(&Frame::Queue { waiting: vec![] });
    a.client.drain_once().await.unwrap();
    b.client.pull_once().await.unwrap();
    assert!(b.store.open_permissions().unwrap().is_empty());

    // A's snapshot omits s1: it was lost on the owner, so B closes it.
    a.store
        .update_session(
            "s1",
            tracon::store::SessionPatch {
                state: Some("closed".into()),
                ..Default::default()
            },
        )
        .unwrap();
    a.client.send_snapshots();
    a.client.drain_once().await.unwrap();
    b.client.pull_once().await.unwrap();
    let s = b.store.get_session("s1").unwrap().unwrap();
    assert_eq!(s.state, "closed");
    assert_eq!(s.last_error.as_deref(), Some("lost on owner"));
}

#[tokio::test]
async fn outbox_survives_a_hub_outage_and_members_are_learned() {
    state::isolate();
    let a_id = Identity::from_seed(&[1u8; 32]);
    let b_id = Identity::from_seed(&[2u8; 32]);
    // A points at a hub that is not listening yet.
    let ring = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let a = node(
        1,
        "alpha",
        "http://127.0.0.1:1",
        &[(MESH_CHANNEL, ring.clone())],
    );
    a.client
        .enqueue(
            MESH_CHANNEL,
            None,
            &Payload::Node(json!({"id": a_id.node_id()})),
        )
        .unwrap();
    assert!(a.client.drain_once().await.is_err());
    assert_eq!(a.client.snapshot().queued, 1);
    assert!(matches!(
        a.client.snapshot().hub,
        HubState::Unreachable { .. }
    ));

    // The real hub comes up; a fresh client on the same store drains it.
    let hub = start_hub(&[(&a_id, &[MESH_CHANNEL]), (&b_id, &[MESH_CHANNEL, "work"])]).await;
    let mut cfg = Config::default();
    cfg.mesh.hub_url = Some(hub.clone());
    let client = MeshClient::new(
        Identity::from_seed(&[1u8; 32]),
        &hub,
        a.store.clone(),
        a.bus.clone(),
        Arc::new(cfg),
        Default::default(),
    );
    assert_eq!(client.drain_once().await.unwrap(), 1);
    assert_eq!(client.snapshot().queued, 0);
    assert!(matches!(client.snapshot().hub, HubState::Connected));

    assert_eq!(client.refresh_members().await.unwrap(), 1);
    assert_eq!(
        a.store.node_channels(&b_id.node_id()).unwrap(),
        vec!["@mesh", "work"]
    );
    let placeholder = a.store.get_node(&b_id.node_id()).unwrap().unwrap();
    assert_eq!(placeholder.state, "unknown");
    assert_eq!(placeholder.reachable, 0);
}

#[tokio::test]
async fn a_meshed_node_refuses_sessions_on_channels_without_keys() {
    state::isolate();
    use tracon::session::{Manager, NewSession, SessionError};
    let id = Identity::from_seed(&[5u8; 32]);
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.put_node(&node_row(&id, "n")).unwrap();
    let mut cfg = Config::default();
    cfg.mesh.hub_url = Some("http://127.0.0.1:1".into());
    let tools = Arc::new(tracon::mcp::Tools {
        broker: Default::default(),
        cfg: Arc::new(cfg.clone()),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        Arc::new(cfg),
        id.node_id(),
        tools,
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let spec = NewSession {
        channel: "work".into(),
        repo_path: "/r".into(),
        branch: None,
        work_item_id: None,
        model: "m".into(),
        budget_tokens: None,
        node_id: None,
        phase: Default::default(),
        review_id: None,
        base_sha: None,
    };
    let adapter: Arc<dyn tracon::adapter::HarnessAdapter> =
        Arc::new(tracon::adapter::omp::OmpAdapter::new(String::from("1")));
    let err = manager.create(spec, adapter).await.unwrap_err();
    assert!(matches!(err, SessionError::UnknownChannel(c) if c == "work"));
    let _ = NewEvent {
        session_id: String::new(),
        work_item_id: None,
        kind: String::new(),
        ref_id: None,
        payload: json!(null),
        at_ms: 0,
        mono_ms: 0,
    };
}

#[tokio::test]
async fn a_credential_handoff_lands_in_the_receivers_sealed_store() {
    state::isolate();
    let (a, b) = pair().await;
    let dir = std::env::temp_dir().join(format!("tracon-mesh-cred-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("credentials.sealed");
    let broker = tracon::broker::Broker::default().shared();
    b.client.set_broker(broker.clone(), path.clone());

    let pinned = tracon::broker::Credential {
        channels: vec!["work".into()],
        nodes: vec![b.id.node_id()],
        env: [("TOKEN".to_string(), "t".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let loose = tracon::broker::Credential {
        channels: vec!["work".into()],
        ..Default::default()
    };
    let payload = Payload::CredentialHandoff {
        credentials: tracon::broker::Broker::handoff_rows(&[
            ("glab".into(), pinned),
            ("mine".into(), loose),
        ]),
    };
    tracon::mesh::enroll::post_direct(
        &a.id,
        a.client.hub_url(),
        &b.id.node_id(),
        &b.id.x25519_public(),
        &payload,
    )
    .await
    .unwrap();

    assert_eq!(b.client.pull_once().await.unwrap(), 1);
    let stored = broker.read().unwrap();
    assert!(stored.get("glab").is_some(), "pinned credential stored");
    assert!(stored.get("mine").is_none(), "unpinned credential dropped");
    // And it is on disk, sealed under B's key, readable back.
    let back = tracon::broker::Broker::load_at(
        &path,
        &dir.join("credentials.toml"),
        &b.id.credential_store_key(),
    )
    .unwrap();
    assert!(back.get("glab").is_some());
    let _ = std::fs::remove_dir_all(&dir);
}
