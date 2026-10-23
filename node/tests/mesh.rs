//! Two nodes and a real hub router, all in one process. The clients' loops are
//! not spawned; the test drives `drain_once` / `pull_once` so every step is
//! deterministic.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::Arc;

use hub::store::{FrameStore, MemberStore};
use proto::envelope::DataKey;
use proto::frame::{Envelope, Payload, MESH_CHANNEL};
use proto::keyring::Keyring;
use proto::keys::Identity;
use serde_json::json;
use tracon::config::Config;
use tracon::mesh::client::MeshClient;
use tracon::mesh::HubState;
use tracon::store::{now_ms, NewEvent, PolicyRolloutRow, Store};
use tracon::stream::{Bus, Frame};

struct Node {
    id: Identity,
    store: Arc<Store>,
    bus: Bus,
    client: Arc<MeshClient>,
}

/// POST one frame the way a node does, so a test can send bytes no node would
/// build.
async fn post_frame(id: &Identity, hub_url: &str, env: &Envelope) -> (u16, String) {
    let body = serde_json::to_vec(env).unwrap();
    let ts = (now_ms() / 1000).max(0) as u64;
    let mut req = reqwest::Client::new().post(format!("{hub_url}/v0/frames"));
    for (k, v) in proto::auth::signed_headers(id, "POST", "/v0/frames", &body, ts) {
        req = req.header(k, v);
    }
    let res = req
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    (res.status().as_u16(), res.text().await.unwrap_or_default())
}

/// The next frame that is not the hub-state banner update.
async fn next_frame(rx: &mut tokio::sync::broadcast::Receiver<Frame>) -> Frame {
    loop {
        // The frame is published from another task; a non-blocking receive
        // here would be a race, not a check.
        let f = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a frame before the deadline")
            .expect("the bus is open");
        if !matches!(f, Frame::Mesh(_)) {
            return f;
        }
    }
}

fn node(seed: u8, name: &str, hub: &str, rings: &[(&str, Keyring)]) -> Node {
    let id = Identity::from_seed(&[seed; 32]);
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .put_node(&{
            let mut r = support::rows::node_row(&id.node_id(), name);
            r.x25519_pub = Some(id.x25519_hex());
            r
        })
        .unwrap();
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
    let (a, b, _) = pair_managed().await;
    (a, b)
}

/// As [`pair`], over a hub whose member and frame stores the test keeps, for
/// revoking a member or planting a frame no honest node would post.
async fn pair_managed() -> (Node, Node, support::mesh::TestHub) {
    let a_id = Identity::from_seed(&[1u8; 32]);
    let b_id = Identity::from_seed(&[2u8; 32]);
    let test_hub = support::mesh::start_hub_managed(
        &[
            (&a_id, &[MESH_CHANNEL, "personal", "secret"][..]),
            (&b_id, &[MESH_CHANNEL, "personal", "secret"][..]),
        ],
        0,
    )
    .await;
    let hub = test_hub.url.clone();
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
    (a, b, test_hub)
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
    let f = next_frame(&mut b_sub).await;
    assert!(matches!(f, Frame::Node(ref v) if v["id"] == a.id.node_id() && v["is_self"] == false));
    assert!(matches!(b.client.snapshot().hub, HubState::Connected));

    // Long after the last hello, the peer dims; the change is published.
    b.client.presence_tick(now_ms() + 3 * 60_000 + 1);
    let peer = b.store.get_node(&a.id.node_id()).unwrap().unwrap();
    assert_eq!(peer.reachable, 0);
    let f = next_frame(&mut b_sub).await;
    assert!(matches!(f, Frame::Node(ref v) if v["reachable"] == false));
    // A's own hello echoed back changes nothing on A.
    assert_eq!(a.client.pull_once().await.unwrap(), 0);
}

#[tokio::test]
async fn sessions_and_events_mirror_once_and_reach_the_bus() {
    state::isolate();
    let (a, b) = pair().await;
    let mut b_sub = b.bus.subscribe();
    let row = support::rows::session_row("s1", &a.id.node_id(), "personal");
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
    assert!(matches!(next_frame(&mut b_sub).await, Frame::Session(_)));
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
    let row = support::rows::session_row("forged", &b.id.node_id(), "personal");
    a.client
        .enqueue("personal", None, &Payload::Session(json!(row)))
        .unwrap();
    a.client.drain_once().await.unwrap();
    assert_eq!(b.client.pull_once().await.unwrap(), 0);
    assert!(b.store.get_session("forged").unwrap().is_none());
    let s = b.client.snapshot();
    assert_eq!(s.undecryptable, 0);
    assert!(s
        .last_refusal
        .as_deref()
        .unwrap()
        .contains("spoke for another node"));

    // A also sends on a channel B has the wrong key for: counted, not applied.
    a.client
        .enqueue(
            "secret",
            None,
            &Payload::Node(json!({"id": a.id.node_id()})),
        )
        .unwrap();
    a.client.drain_once().await.unwrap();
    assert_eq!(b.client.pull_once().await.unwrap(), 0);
    assert_eq!(b.client.snapshot().undecryptable, 1);
}

#[tokio::test]
async fn queue_frames_expire_answered_requests_and_snapshots_close_lost_sessions() {
    state::isolate();
    let (a, b) = pair().await;
    let row = support::rows::session_row("s1", &a.id.node_id(), "personal");
    a.store.insert_session(&row).unwrap();
    a.client.on_frame(&Frame::Session(Box::new(row.clone())));
    let perm = tracon::store::PermissionRow {
        id: "p1".into(),
        session_id: Some("s1".into()),
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
        waiting: vec![perm.clone().into()],
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
    let hub =
        support::mesh::start_hub(&[(&a_id, &[MESH_CHANNEL]), (&b_id, &[MESH_CHANNEL, "work"])])
            .await;
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
    store
        .put_node(&{
            let mut r = support::rows::node_row(&id.node_id(), "n");
            r.x25519_pub = Some(id.x25519_hex());
            r
        })
        .unwrap();
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
        initial_prompt: None,
        node_id: None,
        phase: Default::default(),
        review_id: None,
        base_sha: None,
        workspace_id: None,
        parent_session: None,
        continued_from: None,
        harness: None,
        on_exhaustion: None,
        told_reviews: Vec::new(),
    };
    let adapter: Arc<dyn tracon::adapter::HarnessAdapter> = Arc::new(
        tracon::adapter::claude::ClaudeAdapter::new(String::from("1")),
    );
    let err = manager.create_with(spec, adapter).await.unwrap_err();
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

/// A channel key that arrives after frames sealed under it: the frames are
/// held, named to the operator, and applied the moment the key lands. Dropping
/// them would lose a peer's state with nothing to ask for it back.
#[tokio::test]
async fn a_frame_that_precedes_its_key_is_held_and_applied_when_the_key_lands() {
    state::isolate();
    let (a, b) = pair().await;
    // A rotates `personal`; its next frame is sealed under an epoch B has not
    // been handed.
    let ring =
        Keyring::from_bytes(&a.store.channel_get("personal").unwrap().unwrap().keyring).unwrap();
    let (rotated, _) = ring.rotate(&a.id.x25519_public(), 1000);
    a.store
        .channel_put("personal", &rotated.to_bytes(), "{}")
        .unwrap();
    let row = support::rows::session_row("s1", &a.id.node_id(), "personal");
    a.client
        .enqueue("personal", None, &Payload::Session(json!(row)))
        .unwrap();
    assert_eq!(a.client.drain_once().await.unwrap(), 1);

    // B cannot open it. It is held, counted, and named — not dropped.
    assert_eq!(b.client.pull_once().await.unwrap(), 0);
    assert!(b.store.get_session("s1").unwrap().is_none());
    let s = b.client.snapshot();
    assert_eq!(s.held, 1);
    assert_eq!(s.undecryptable, 1);
    assert!(
        s.last_refusal
            .as_deref()
            .unwrap()
            .contains("held until its channel key arrives"),
        "{s:?}"
    );
    // Pulling again moves the cursor past it without dropping or recounting it.
    assert_eq!(b.client.pull_once().await.unwrap(), 0);
    assert_eq!(b.client.snapshot().held, 1);
    assert_eq!(b.client.snapshot().undecryptable, 1);

    // The key arrives; what was waiting on it is applied at once.
    let handoff = tracon::mesh::enroll::handoff_payload(
        &a.store,
        &a.id,
        &b.id.x25519_public(),
        &["personal".into()],
    )
    .unwrap();
    tracon::mesh::enroll::post_direct(
        &a.id,
        a.client.hub_url(),
        &b.id.node_id(),
        &b.id.x25519_public(),
        &handoff,
    )
    .await
    .unwrap();
    // B takes a key only from a member of the channel by its own record.
    b.client.refresh_members().await.unwrap();
    assert!(b.client.pull_once().await.unwrap() >= 1);
    let mirrored = b.store.get_session("s1").unwrap().unwrap();
    assert_eq!(mirrored.node_id, a.id.node_id());
    assert_eq!(mirrored.channel, "personal");
    assert_eq!(b.client.snapshot().held, 0);
    // And applying it once is enough: a further pull is a no-op.
    assert_eq!(b.client.pull_once().await.unwrap(), 0);
}

/// A member the hub has removed: its frames are refused where it can see the
/// reason, nothing it says reaches the mesh, no further key reaches it, and the
/// nodes that stay drop its channel grants rather than keeping a stale one.
#[tokio::test]
async fn a_removed_member_is_refused_visibly_and_is_handed_no_further_keys() {
    state::isolate();
    let (a, b, hub) = pair_managed().await;
    let (ai, bi) = (a.id.node_id(), b.id.node_id());
    // Before removal: B speaks and A hears it, and A records B's grants.
    let row = support::rows::session_row("before", &bi, "personal");
    b.store.insert_session(&row).unwrap();
    b.client
        .enqueue("personal", None, &Payload::Session(json!(row)))
        .unwrap();
    assert_eq!(b.client.drain_once().await.unwrap(), 1);
    assert_eq!(a.client.pull_once().await.unwrap(), 1);
    assert_eq!(a.client.refresh_members().await.unwrap(), 1);
    assert_eq!(
        a.store.node_channels(&bi).unwrap(),
        vec!["@mesh", "personal", "secret"]
    );

    // The lost laptop is revoked on the hub.
    assert!(hub.members.remove(&bi).unwrap());

    // B's next frame is refused, in terms B's operator can read, and dropped
    // rather than left to block everything behind it.
    let after = support::rows::session_row("after", &bi, "personal");
    b.client
        .enqueue("personal", None, &Payload::Session(json!(after)))
        .unwrap();
    assert_eq!(b.client.drain_once().await.unwrap(), 0);
    assert_eq!(b.client.snapshot().queued, 0);
    let s = b.client.snapshot();
    assert!(
        s.last_refusal
            .as_deref()
            .unwrap()
            .contains("not a member of this hub"),
        "{s:?}"
    );
    assert_eq!(a.client.pull_once().await.unwrap(), 0);
    assert!(a.store.get_session("after").unwrap().is_none());

    // A key handed to it after the removal never reaches it: B holds the same
    // `secret` keyring it started with, and its own pull is refused outright.
    let before_keys = b.store.channel_get("secret").unwrap().unwrap().keyring;
    let handoff = tracon::mesh::enroll::handoff_payload(
        &a.store,
        &a.id,
        &b.id.x25519_public(),
        &["secret".into()],
    )
    .unwrap();
    tracon::mesh::enroll::post_direct(
        &a.id,
        a.client.hub_url(),
        &bi,
        &b.id.x25519_public(),
        &handoff,
    )
    .await
    .unwrap();
    assert_eq!(b.client.pull_once().await.unwrap(), 0);
    assert_eq!(
        b.store.channel_get("secret").unwrap().unwrap().keyring,
        before_keys
    );

    // And the member list is a snapshot, not an add list: A's record of what B
    // may speak on is cleared, so no stale row outlives the revocation.
    assert_eq!(a.client.refresh_members().await.unwrap(), 0);
    assert!(a.store.node_channels(&bi).unwrap().is_empty());
    assert!(a.client.peer_key(&bi).is_none());
    let _ = ai;
}

/// A peer that leaves a channel lists nothing there again, so the cards this
/// node mirrored from it there stayed open, and an answer went to a peer that
/// refuses it from a node no longer beside it (#610). The member list that
/// says it left closes them, with a session or without, and only there.
#[tokio::test]
async fn a_peers_cards_on_a_channel_it_left_are_closed_by_the_member_list() {
    state::isolate();
    let (a, b, hub) = pair_managed().await;
    let bi = b.id.node_id();
    assert_eq!(a.client.refresh_members().await.unwrap(), 1);
    a.store
        .upsert_session_mirror(&support::rows::session_row("s-b", &bi, "personal"))
        .unwrap();
    let card = |id: &str, session: Option<&str>| tracon::store::PermissionRow {
        id: id.into(),
        session_id: session.map(String::from),
        node_id: bi.clone(),
        rpc_id: 0,
        tool_call_id: None,
        title: "pr_comment o/r#7".into(),
        kind: None,
        raw_input: None,
        options: "[]".into(),
        state: "new".into(),
        answer_option_id: None,
        created_ms: now_ms(),
        created_mono_ms: 0,
        resolved_mono_ms: None,
        expires_ms: now_ms() + 600_000,
    };
    for (id, session, channel) in [
        ("bound", Some("s-b"), "personal"),
        ("sessionless", None, "personal"),
        ("elsewhere", None, "secret"),
    ] {
        assert!(a
            .store
            .upsert_permission_mirror(&card(id, session), channel)
            .unwrap());
    }
    assert_eq!(a.store.open_permission_views().unwrap().len(), 3);

    let mut rx = a.bus.subscribe();
    let mut member = hub.members.get(&bi).unwrap().unwrap();
    member.channels = vec![MESH_CHANNEL.into(), "secret".into()];
    hub.members.put(&member).unwrap();
    a.client.refresh_members().await.unwrap();

    let open: Vec<String> = a
        .store
        .open_permission_views()
        .unwrap()
        .iter()
        .map(|p| p.id.clone())
        .collect();
    assert_eq!(open, vec!["elsewhere"]);
    for id in ["bound", "sessionless"] {
        assert_eq!(
            a.store.get_permission(id).unwrap().unwrap().state,
            "expired"
        );
    }
    // And the interface is told, so the closed cards leave Home.
    let waiting = loop {
        if let Frame::Queue { waiting } = next_frame(&mut rx).await {
            break waiting;
        }
    };
    assert_eq!(waiting.len(), 1, "{waiting:?}");
}

/// A peer the hub removes outright is the only way one leaves a channel today:
/// the hub merges channels on admit and never takes one away. The departed
/// loop of the member list closes its cards on every channel, with a session
/// or without, and tells the interface (#610).
#[tokio::test]
async fn a_removed_peers_cards_are_closed_by_the_member_list() {
    state::isolate();
    let (a, b, hub) = pair_managed().await;
    let bi = b.id.node_id();
    assert_eq!(a.client.refresh_members().await.unwrap(), 1);
    a.store
        .upsert_session_mirror(&support::rows::session_row("s-b", &bi, "personal"))
        .unwrap();
    let card = |id: &str, session: Option<&str>| tracon::store::PermissionRow {
        id: id.into(),
        session_id: session.map(String::from),
        node_id: bi.clone(),
        rpc_id: 0,
        tool_call_id: None,
        title: "pr_comment o/r#7".into(),
        kind: None,
        raw_input: None,
        options: "[]".into(),
        state: "new".into(),
        answer_option_id: None,
        created_ms: now_ms(),
        created_mono_ms: 0,
        resolved_mono_ms: None,
        expires_ms: now_ms() + 600_000,
    };
    for (id, session) in [("bound", Some("s-b")), ("sessionless", None)] {
        assert!(a
            .store
            .upsert_permission_mirror(&card(id, session), "personal")
            .unwrap());
    }
    assert_eq!(a.store.open_permission_views().unwrap().len(), 2);

    let mut rx = a.bus.subscribe();
    assert!(hub.members.remove(&bi).unwrap());
    assert_eq!(a.client.refresh_members().await.unwrap(), 0);

    for id in ["bound", "sessionless"] {
        assert_eq!(
            a.store.get_permission(id).unwrap().unwrap().state,
            "expired"
        );
    }
    // And the interface is told, so the closed cards leave Home.
    let waiting = loop {
        if let Frame::Queue { waiting } = next_frame(&mut rx).await {
            break waiting;
        }
    };
    assert!(waiting.is_empty(), "{waiting:?}");
}

/// A frame from a build on another wire version is refused by name. The hub
/// never stores one; a node that is handed one anyway says why rather than
/// opening it or holding it for a key.
#[tokio::test]
async fn a_frame_from_another_wire_version_is_refused_with_a_reason() {
    state::isolate();
    let (a, b, hub) = pair_managed().await;
    let ring =
        Keyring::from_bytes(&a.store.channel_get("personal").unwrap().unwrap().keyring).unwrap();
    let row = support::rows::session_row("newer", &a.id.node_id(), "personal");
    let mut env = Envelope::seal_channel(
        &a.id,
        "personal",
        None,
        &ring,
        &Payload::Session(json!(row)),
        now_ms(),
    )
    .unwrap();
    env.v = proto::CONTRACT_VERSION + 1;

    // The hub refuses to relay it, saying which versions disagree.
    let (status, body) = post_frame(&a.id, a.client.hub_url(), &env).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("wire version"), "{body}");

    // Planted past the hub, the reader refuses it too, and says so where the
    // operator looks.
    hub.frames
        .append("personal", &serde_json::to_string(&env).unwrap(), now_ms())
        .unwrap();
    assert_eq!(b.client.pull_once().await.unwrap(), 0);
    assert!(b.store.get_session("newer").unwrap().is_none());
    let s = b.client.snapshot();
    assert!(
        s.last_refusal.as_deref().unwrap().contains("wire version"),
        "{s:?}"
    );
    // Refused, not held: no key will ever make this frame readable.
    assert_eq!(s.held, 0);
}

/// A rollout of `toml` from A to B, signed or not: delivery bookkeeping never
/// looks at the signature, only the receiver does.
fn policy_rollout(
    a: &Node,
    b: &Node,
    id: &str,
    toml: &str,
    sig_hex: &str,
    pubkey_hex: &str,
) -> PolicyRolloutRow {
    let version: tracon::policy::Policy = toml::from_str(toml).unwrap();
    let row = PolicyRolloutRow {
        id: id.into(),
        bundle_sha256: tracon::policy::bundle::sha256(toml),
        policy_version: i64::from(version.version),
        toml: toml.into(),
        sig_hex: sig_hex.into(),
        pubkey_hex: pubkey_hex.into(),
        source_node: a.id.node_id(),
        created_ms: now_ms(),
        updated_ms: now_ms(),
    };
    a.store
        .create_policy_rollout(&row, &[b.id.node_id()])
        .unwrap();
    row
}

fn target(node: &Node, rollout: &str) -> tracon::store::PolicyRolloutTargetRow {
    node.store
        .policy_rollout_targets(rollout)
        .unwrap()
        .remove(0)
}

#[tokio::test]
async fn a_rollout_is_queued_for_a_peer_that_is_offline_once_its_key_is_known() {
    state::isolate();
    let (a, b) = pair().await;
    let rollout = policy_rollout(&a, &b, "r1", "version = 7\n", "00", &"11".repeat(32));

    // No sealing key for B yet: nothing can be queued, and it says so.
    a.client.dispatch_policy_rollout("r1").unwrap();
    let t = target(&a, "r1");
    assert_eq!(t.status, "offline");
    let detail = t.detail.unwrap();
    assert!(
        detail.starts_with("Not queued: no sealing key known for")
            && detail.ends_with("Retried automatically when the peer is reachable."),
        "{detail}"
    );
    assert_eq!(a.client.snapshot().queued, 0);

    // The member list gives A the key, and reaching the hub is itself a
    // reason to reconcile. B has never said hello, so it is not reachable,
    // and the bundle is queued for the hub to hold all the same.
    a.client.refresh_members().await.unwrap();
    assert!(!a.client.peer_reachable(&b.id.node_id()));
    let t = target(&a, &rollout.id);
    assert_eq!(t.status, "sent");
    assert_eq!(t.attempts, 1);
    assert_eq!(
        t.detail.as_deref(),
        Some(
            "Queued at the hub; delivered when the peer next connects. Installation not confirmed."
        )
    );
    assert_eq!(a.client.snapshot().queued, 1);
}

#[tokio::test]
async fn a_returning_peer_is_sent_what_could_not_be_queued_for_it() {
    state::isolate();
    let (a, b) = pair().await;
    policy_rollout(&a, &b, "r1", "version = 7\n", "00", &"11".repeat(32));
    a.client.dispatch_policy_rollout("r1").unwrap();
    assert_eq!(target(&a, "r1").status, "offline");

    // B's hello brings its key and makes it reachable; that is the moment.
    b.client.hello().await.unwrap();
    a.client.pull_once().await.unwrap();
    assert_eq!(target(&a, "r1").status, "sent");

    // Gone quiet, then heard from again: a presence tick that finds it fresh
    // retries what was left unqueued in between.
    a.client.presence_tick(now_ms() + 60 * 60_000);
    assert!(!a.client.peer_reachable(&b.id.node_id()));
    a.store
        .mark_policy_rollout_offline("r1", &b.id.node_id(), "outbox error")
        .unwrap();
    a.client.presence_tick(now_ms());
    let t = target(&a, "r1");
    assert_eq!(t.status, "sent");
    assert_eq!(t.attempts, 2);
}

#[tokio::test]
async fn an_unconfirmed_rollout_is_resent_only_after_the_wait_and_only_to_a_reachable_peer() {
    state::isolate();
    let (a, b) = pair().await;
    b.client.hello().await.unwrap();
    a.client.pull_once().await.unwrap();
    let peer = b.id.node_id();
    assert!(a.client.peer_reachable(&peer));
    policy_rollout(&a, &b, "r1", "version = 7\n", "00", &"11".repeat(32));
    a.client.dispatch_policy_rollout("r1").unwrap();
    assert_eq!(target(&a, "r1").attempts, 1);

    let wait = tracon::mesh::client::RESEND_AFTER.as_millis() as i64;
    let now = now_ms();
    assert_eq!(
        a.client
            .reconcile_policy_rollouts_at(Some(&peer), now + wait / 2),
        0
    );
    assert_eq!(
        a.client
            .reconcile_policy_rollouts_at(Some(&peer), now + wait + 1),
        1
    );
    assert_eq!(target(&a, "r1").attempts, 2);

    // An unreachable peer is not sent the same frame over and over: the hub
    // already holds one for it.
    a.client.presence_tick(now + 2 * wait);
    assert!(!a.client.peer_reachable(&peer));
    assert_eq!(
        a.client
            .reconcile_policy_rollouts_at(Some(&peer), now + 3 * wait),
        0
    );
    assert_eq!(target(&a, "r1").attempts, 2);
}

#[tokio::test]
async fn a_peers_signed_hello_naming_the_bundle_confirms_the_rollout() {
    state::isolate();
    let (a, b) = pair().await;
    a.client.refresh_members().await.unwrap();
    let identity = "11".repeat(32);
    let rollout = policy_rollout(&a, &b, "r1", "version = 7\n", "00", &identity);
    a.client.dispatch_policy_rollout("r1").unwrap();
    assert_eq!(target(&a, "r1").status, "sent");

    // B's receipt never arrived; its hello says what it runs.
    b.store
        .set_node_policy_metadata(
            &b.id.node_id(),
            Some(&identity),
            Some(&rollout.bundle_sha256),
            true,
        )
        .unwrap();
    b.client.hello().await.unwrap();
    a.client.pull_once().await.unwrap();
    let t = target(&a, "r1");
    assert_eq!(t.status, "applied");
    assert!(t.acknowledged_ms.is_some());
    assert_eq!(
        t.detail.as_deref(),
        Some("Confirmed by the peer's signed hello.")
    );
    // Confirmed, so nothing is ever resent.
    assert_eq!(
        a.client
            .reconcile_policy_rollouts_at(None, now_ms() + 24 * 60 * 60_000),
        0
    );
}

/// The only test in this file that writes policy files: the state directory
/// is shared by the whole binary, and both nodes read the one bundle in it.
#[tokio::test]
async fn a_receiver_refuses_older_and_conflicting_bundles_and_takes_its_own_again_quietly() {
    use tracon::policy::bundle;
    state::isolate();
    let (a, b) = pair().await;
    let (signing, _) = bundle::generate_key();
    bundle::write_key(&signing).unwrap();
    let identity = bundle::public_identity().unwrap();
    let installed = "version = 21\n";
    bundle::install(
        installed,
        &bundle::sign(installed).unwrap(),
        &identity,
        false,
    )
    .unwrap();
    a.client.refresh_members().await.unwrap();
    b.client.refresh_members().await.unwrap();

    for (id, toml, applied, detail) in [
        (
            "older",
            "version = 20\n",
            false,
            Some("older than installed v21"),
        ),
        ("same", installed, true, None),
        (
            "conflicting",
            "version = 21\n# another\n",
            false,
            Some("conflicting bundle for v21"),
        ),
    ] {
        policy_rollout(&a, &b, id, toml, &bundle::sign(toml).unwrap(), &identity);
        a.client.dispatch_policy_rollout(id).unwrap();
        a.client.drain_once().await.unwrap();
        b.client.pull_once().await.unwrap();
        b.client.drain_once().await.unwrap();
        a.client.pull_once().await.unwrap();
        let t = target(&a, id);
        assert_eq!(
            t.status,
            if applied { "applied" } else { "rejected" },
            "{id}"
        );
        assert_eq!(t.detail.as_deref(), detail, "{id}");
    }
    // Nothing was written: the same bundle is a no-op, the others refusals.
    assert_eq!(bundle::current().unwrap().toml, installed);
    assert!(b.store.policy_installation().unwrap().is_none());
}

/// B's broker, sealed in a directory of its own, holding `glab` as A handed
/// it over: the copy a revoke from A is for.
async fn shared_from_a(
    a: &Node,
    b: &Node,
    tag: &str,
) -> (tracon::broker::SharedBroker, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("tracon-mesh-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let broker = tracon::broker::Broker::default().shared();
    b.client
        .set_broker(broker.clone(), dir.join("credentials.sealed"));
    // A's hello gives B the key its receipts are sealed to.
    a.client.hello().await.unwrap();
    let glab = tracon::broker::Credential {
        channels: vec!["work".into()],
        nodes: vec![a.id.node_id(), b.id.node_id()],
        grant_id: Some("0190-a".into()),
        ..Default::default()
    };
    tracon::mesh::enroll::post_direct(
        &a.id,
        a.client.hub_url(),
        &b.id.node_id(),
        &b.id.x25519_public(),
        &Payload::CredentialHandoff {
            credentials: tracon::broker::Broker::handoff_rows(&[("glab".into(), glab)]),
        },
    )
    .await
    .unwrap();
    b.client.pull_once().await.unwrap();
    assert!(broker.read().unwrap().get("glab").is_some());
    (broker, dir)
}

fn revoke_status(node: &Node, revoke_id: &str) -> String {
    node.store
        .credential_revoke_targets(revoke_id)
        .unwrap()
        .remove(0)
        .status
}

#[tokio::test]
async fn a_revoke_reaches_a_peer_which_drops_its_copy_and_confirms() {
    state::isolate();
    let (a, b) = pair().await;
    b.client.hello().await.unwrap();
    a.client.pull_once().await.unwrap();
    let (broker, dir) = shared_from_a(&a, &b, "revoke").await;
    let dropped = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let told = dropped.clone();
    b.client.set_on_revoke(Box::new(move |names| {
        told.lock().unwrap().extend_from_slice(names)
    }));

    let id = a
        .store
        .create_credential_revoke("glab", Some("0190-a"), 0, &[b.id.node_id()])
        .unwrap();
    a.client.dispatch_credential_revoke(&id).unwrap();
    assert_eq!(revoke_status(&a, &id), "sent");
    a.client.drain_once().await.unwrap();
    b.client.pull_once().await.unwrap();

    assert!(broker.read().unwrap().get("glab").is_none());
    assert_eq!(*dropped.lock().unwrap(), ["glab"]);
    // Sealed without it, and the tombstone with it.
    let back = tracon::broker::Broker::load_at(
        &dir.join("credentials.sealed"),
        &dir.join("credentials.toml"),
        &b.id.credential_store_key(),
    )
    .unwrap();
    assert!(back.get("glab").is_none());
    assert!(back.tombstone("glab").is_some());

    b.client.drain_once().await.unwrap();
    a.client.pull_once().await.unwrap();
    assert_eq!(revoke_status(&a, &id), "confirmed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_revoke_sent_while_the_peer_was_offline_arrives_when_it_returns() {
    state::isolate();
    let (a, b) = pair().await;
    let (broker, dir) = shared_from_a(&a, &b, "revoke-offline").await;

    // A has never heard from B: nothing can be sealed to it yet.
    let id = a
        .store
        .create_credential_revoke("glab", Some("0190-a"), 0, &[b.id.node_id()])
        .unwrap();
    a.client.dispatch_credential_revoke(&id).unwrap();
    assert_eq!(revoke_status(&a, &id), "offline");

    // B's hello brings its key and makes it reachable; the revoke follows.
    b.client.hello().await.unwrap();
    a.client.pull_once().await.unwrap();
    assert_eq!(revoke_status(&a, &id), "sent");
    a.client.drain_once().await.unwrap();
    b.client.pull_once().await.unwrap();
    assert!(broker.read().unwrap().get("glab").is_none());
    b.client.drain_once().await.unwrap();
    a.client.pull_once().await.unwrap();
    assert_eq!(revoke_status(&a, &id), "confirmed");

    // Confirmed is final: nothing is resent, however long it has been.
    let wait = tracon::mesh::client::RESEND_AFTER.as_millis() as i64;
    assert_eq!(
        a.client
            .reconcile_credential_revokes_at(None, now_ms() + 3 * wait),
        0
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review of #608, must-fix 2: a peer that keeps its copy answers so, and
/// the revoke shows it as kept rather than confirmed. Here B holds `glab`
/// under its own sign-in.
#[tokio::test]
async fn a_peer_that_keeps_its_own_copy_answers_kept() {
    state::isolate();
    let (a, b) = pair().await;
    b.client.hello().await.unwrap();
    a.client.pull_once().await.unwrap();
    a.client.hello().await.unwrap();
    let dir = std::env::temp_dir().join(format!("tracon-mesh-revoke-kept-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let broker = tracon::broker::Broker::default().shared();
    let mut own = tracon::broker::Credential {
        nodes: vec![b.id.node_id(), a.id.node_id()],
        ..Default::default()
    };
    own.ensure_grant(&b.id.node_id());
    broker.write().unwrap().put("glab", own);
    b.client
        .set_broker(broker.clone(), dir.join("credentials.sealed"));

    let id = a
        .store
        .create_credential_revoke("glab", Some("0190-a"), 0, &[b.id.node_id()])
        .unwrap();
    a.client.dispatch_credential_revoke(&id).unwrap();
    a.client.drain_once().await.unwrap();
    b.client.pull_once().await.unwrap();
    assert!(broker.read().unwrap().get("glab").is_some());
    b.client.drain_once().await.unwrap();
    a.client.pull_once().await.unwrap();
    let target = a.store.credential_revoke_targets(&id).unwrap().remove(0);
    assert!(target.kept(), "{target:?}");
    assert_eq!(target.outcome.as_deref(), Some("kept"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A revoke a reachable peer never answers is sent a bounded number of
/// times and then abandoned, not resent every half hour for good.
#[tokio::test]
async fn an_unanswered_revoke_is_abandoned_after_its_last_send() {
    state::isolate();
    let (a, b) = pair().await;
    b.client.hello().await.unwrap();
    a.client.pull_once().await.unwrap();
    let id = a
        .store
        .create_credential_revoke("glab", Some("0190-a"), 0, &[b.id.node_id()])
        .unwrap();
    let wait = tracon::mesh::client::RESEND_AFTER.as_millis() as i64;
    let mut now = now_ms();
    for _ in 0..tracon::store::REVOKE_MAX_ATTEMPTS {
        assert_eq!(a.client.reconcile_credential_revokes_at(None, now), 1);
        now += 2 * wait;
    }
    assert_eq!(a.client.reconcile_credential_revokes_at(None, now), 0);
    let target = a.store.credential_revoke_targets(&id).unwrap().remove(0);
    assert!(target.abandoned(), "{target:?}");
    assert_eq!(target.attempts, tracon::store::REVOKE_MAX_ATTEMPTS);
    assert_eq!(
        a.client
            .reconcile_credential_revokes_at(None, now + 2 * wait),
        0
    );
}

/// Third review of #608, nit 4: a peer abandoned for never answering, say
/// on a build that could not read the revoke, is sent it again from a fresh
/// count when it goes quiet and comes back, perhaps upgraded.
#[tokio::test]
async fn an_abandoned_revoke_is_sent_again_when_its_peer_comes_back() {
    state::isolate();
    let (a, b) = pair().await;
    b.client.hello().await.unwrap();
    a.client.pull_once().await.unwrap();
    let peer = b.id.node_id();
    let id = a
        .store
        .create_credential_revoke("glab", Some("0190-a"), 0, std::slice::from_ref(&peer))
        .unwrap();
    assert!(a.store.abandon_credential_revoke(&id, &peer).unwrap());

    // Still reachable: nothing is sent.
    a.client.presence_tick(now_ms());
    assert_eq!(revoke_status(&a, &id), "abandoned");
    a.client.presence_tick(now_ms() + 60 * 60_000);
    assert!(!a.client.peer_reachable(&peer));
    assert_eq!(revoke_status(&a, &id), "abandoned");

    // Heard from again: the revoke goes out anew.
    a.client.presence_tick(now_ms());
    let target = a.store.credential_revoke_targets(&id).unwrap().remove(0);
    assert_eq!(target.status, "sent", "{target:?}");
    assert_eq!(target.attempts, 1);
}

/// Second review of #608, nit 4: a peer nothing can be sealed to, such as
/// one that left the mesh, is tried at each reconcile and abandoned after
/// as many tries as a peer that never answers, not left pending for good.
#[tokio::test]
async fn a_revoke_that_can_never_be_sealed_is_abandoned_too() {
    state::isolate();
    let (a, b) = pair().await;
    // A has never heard from B, so it knows no key to seal to.
    let id = a
        .store
        .create_credential_revoke("glab", Some("0190-a"), 0, &[b.id.node_id()])
        .unwrap();
    let mut now = now_ms();
    for _ in 0..tracon::store::REVOKE_MAX_ATTEMPTS {
        assert_eq!(a.client.reconcile_credential_revokes_at(None, now), 0);
        assert_eq!(revoke_status(&a, &id), "offline");
        now += 60_000;
    }
    assert_eq!(a.client.reconcile_credential_revokes_at(None, now), 0);
    let target = a.store.credential_revoke_targets(&id).unwrap().remove(0);
    assert!(target.abandoned(), "{target:?}");
    assert_eq!(target.attempts, tracon::store::REVOKE_MAX_ATTEMPTS);
    assert!(a
        .store
        .credential_revoke_reconcile_targets(None)
        .unwrap()
        .is_empty());
}
